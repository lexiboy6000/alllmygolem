//! Native OS input backend (real cursor + keyboard) via `enigo`. Used for the
//! Native/Hybrid input strategy and for OS-level dialogs (file open/save) that
//! CDP cannot touch. Implements [`InputBackend`].
//!
//! `enigo` is synchronous and its [`Enigo`] handle is not safe to move across
//! `.await` points or between threads. We therefore confine it to a single
//! dedicated OS thread: the async trait methods package each request into an
//! [`InputCmd`] (carrying a `oneshot` reply channel), push it onto a
//! `std::sync::mpsc` queue, and await the reply. The worker thread owns the
//! `Enigo` for its whole lifetime.
//!
//! If `Enigo` cannot be created (e.g. a locked-down Wayland session), the
//! worker thread stays alive but every command replies with
//! [`GolemError::Input`]; [`NativeInput::new`] still returns `Ok` so the app
//! keeps running, and [`InputBackend::is_available`] reports `false`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use enigo::{
    Axis, Button, Coordinate,
    Direction::{self, Click, Press, Release},
    Enigo, Key, Keyboard, Mouse, Settings,
};
use parking_lot::Mutex;
use tokio::sync::oneshot;
use tracing::warn;

use crate::backend::{InputBackend, MouseButton};
use crate::error::{GolemError, Result};

/// A unit of work for the enigo worker thread. Each variant carries its
/// arguments plus a `oneshot` sender the worker uses to return the outcome.
enum InputCmd {
    MoveAbs {
        x: i32,
        y: i32,
        reply: oneshot::Sender<Result<()>>,
    },
    Button {
        button: MouseButton,
        direction: Direction,
        reply: oneshot::Sender<Result<()>>,
    },
    CursorPos {
        reply: oneshot::Sender<Result<(i32, i32)>>,
    },
    Scroll {
        dx: i32,
        dy: i32,
        reply: oneshot::Sender<Result<()>>,
    },
    Key {
        key: Key,
        direction: Direction,
        reply: oneshot::Sender<Result<()>>,
    },
}

/// Native OS input via a dedicated enigo worker thread.
pub struct NativeInput {
    /// Queue feeding the worker thread. `std::sync::mpsc::Sender` is `Send` but
    /// not `Sync`, so it lives behind a mutex to satisfy `InputBackend: Sync`.
    cmd_tx: Mutex<std::sync::mpsc::Sender<InputCmd>>,
    /// Whether enigo initialized successfully on this session.
    available: AtomicBool,
}

impl NativeInput {
    /// Construct the native backend. Spawns the enigo worker thread and blocks
    /// (briefly) until it reports whether injection is possible. Always returns
    /// `Ok`; check [`InputBackend::is_available`] for the probe result.
    pub fn new() -> Result<Arc<NativeInput>> {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<InputCmd>();
        // The worker reports its init result over this synchronous channel so
        // `new` (which is not async) can learn availability without blocking a
        // tokio runtime.
        let (init_tx, init_rx) = std::sync::mpsc::channel::<bool>();

        let spawn_result = std::thread::Builder::new()
            .name("golem-native-input".to_string())
            .spawn(move || worker_main(cmd_rx, &init_tx));

        if let Err(e) = spawn_result {
            warn!("failed to spawn native input worker thread: {e}");
            // The receiver is dropped with `cmd_rx`; sends will fail and surface
            // as `GolemError::Input`, which is the desired behavior.
            return Ok(Arc::new(NativeInput {
                cmd_tx: Mutex::new(cmd_tx),
                available: AtomicBool::new(false),
            }));
        }

        // Block until the worker reports availability. If the channel closes
        // before a value arrives, treat the backend as unavailable.
        let available = init_rx.recv().unwrap_or(false);
        if !available {
            warn!("native input backend unavailable on this session");
        }

        Ok(Arc::new(NativeInput {
            cmd_tx: Mutex::new(cmd_tx),
            available: AtomicBool::new(available),
        }))
    }

    /// Enqueue a command for the worker thread.
    fn send(&self, cmd: InputCmd) -> Result<()> {
        self.cmd_tx
            .lock()
            .send(cmd)
            .map_err(|_| GolemError::Input("native input worker thread is gone".to_string()))
    }

    /// Build a command from a fresh reply channel, send it, and await the
    /// flattened result.
    async fn dispatch<T, F>(&self, build: F) -> Result<T>
    where
        T: Send,
        F: FnOnce(oneshot::Sender<Result<T>>) -> InputCmd,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send(build(reply_tx))?;
        match reply_rx.await {
            Ok(result) => result,
            Err(_) => Err(GolemError::Input(
                "native input worker dropped the reply".to_string(),
            )),
        }
    }
}

/// Worker thread entry point. Creates the `Enigo` handle, reports availability,
/// then serves commands until the queue closes. Never panics: every enigo error
/// is mapped and returned over the reply channel.
fn worker_main(rx: std::sync::mpsc::Receiver<InputCmd>, init_tx: &std::sync::mpsc::Sender<bool>) {
    let mut enigo = match Enigo::new(&Settings::default()) {
        Ok(handle) => {
            let _ = init_tx.send(true);
            Some(handle)
        }
        Err(e) => {
            warn!("enigo initialization failed; native input disabled: {e}");
            let _ = init_tx.send(false);
            None
        }
    };
    let scale = wayland_coordinate_scale();

    while let Ok(cmd) = rx.recv() {
        match cmd {
            InputCmd::MoveAbs { x, y, reply } => {
                let (x, y) = (
                    (f64::from(x) * scale).round() as i32,
                    (f64::from(y) * scale).round() as i32,
                );
                let res = match enigo.as_mut() {
                    Some(e) => e.move_mouse(x, y, Coordinate::Abs).map_err(map_enigo_err),
                    None => Err(unavailable()),
                };
                let _ = reply.send(res);
            }
            InputCmd::Button {
                button,
                direction,
                reply,
            } => {
                let res = match enigo.as_mut() {
                    Some(e) => e.button(map_button(button), direction).map_err(map_enigo_err),
                    None => Err(unavailable()),
                };
                let _ = reply.send(res);
            }
            InputCmd::CursorPos { reply } => {
                let res = match enigo.as_ref() {
                    Some(e) => e.location().map_err(map_enigo_err),
                    None => Err(unavailable()),
                };
                let _ = reply.send(res);
            }
            InputCmd::Scroll { dx, dy, reply } => {
                let res = match enigo.as_mut() {
                    Some(e) => do_scroll(e, dx, dy),
                    None => Err(unavailable()),
                };
                let _ = reply.send(res);
            }
            InputCmd::Key {
                key,
                direction,
                reply,
            } => {
                let res = match enigo.as_mut() {
                    Some(e) => e.key(key, direction).map_err(map_enigo_err),
                    None => Err(unavailable()),
                };
                let _ = reply.send(res);
            }
        }
    }
}

/// Scroll on each requested axis. `enigo`'s scroll length is in wheel "clicks"
/// (15° rotations); positive vertical scrolls down, positive horizontal scrolls
/// right. Surfaces the first error encountered.
fn do_scroll(enigo: &mut Enigo, dx: i32, dy: i32) -> Result<()> {
    if dx != 0 {
        enigo
            .scroll(dx, Axis::Horizontal)
            .map_err(map_enigo_err)?;
    }
    if dy != 0 {
        enigo.scroll(dy, Axis::Vertical).map_err(map_enigo_err)?;
    }
    Ok(())
}

/// Factor to convert the logical screen coordinates the rest of the app works
/// in into the coordinates enigo's Wayland virtual-pointer expects.
///
/// enigo expresses absolute positions as a fraction of the output's PHYSICAL
/// mode size, but the compositor maps that fraction onto the output's LOGICAL
/// size — so on a scaled monitor (e.g. 1.5x HiDPI) every move lands short by
/// the scale factor unless we pre-multiply. Verified empirically on Hyprland:
/// asking for (600,450) on a 1.5x monitor put the cursor at (400,300).
///
/// Only Hyprland is queried (`hyprctl monitors -j`, focused monitor); on X11
/// sessions or other compositors this returns 1.0 (no adjustment).
fn wayland_coordinate_scale() -> f64 {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return 1.0;
    }
    let Ok(out) = std::process::Command::new("hyprctl")
        .args(["monitors", "-j"])
        .output()
    else {
        return 1.0;
    };
    if !out.status.success() {
        return 1.0;
    }
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return 1.0;
    };
    let Some(monitors) = v.as_array() else {
        return 1.0;
    };
    monitors
        .iter()
        .find(|m| m.get("focused").and_then(serde_json::Value::as_bool) == Some(true))
        .or_else(|| monitors.first())
        .and_then(|m| m.get("scale").and_then(serde_json::Value::as_f64))
        .filter(|s| *s > 0.0)
        .unwrap_or(1.0)
}

/// Error returned for every command when enigo could not initialize.
fn unavailable() -> GolemError {
    GolemError::Input(
        "native input unavailable: enigo could not initialize on this session".to_string(),
    )
}

/// Map an enigo input error into the crate error type.
fn map_enigo_err(e: enigo::InputError) -> GolemError {
    GolemError::Input(format!("{e}"))
}

/// Map the backend-neutral mouse button to enigo's enum.
fn map_button(button: MouseButton) -> Button {
    match button {
        MouseButton::Left => Button::Left,
        MouseButton::Right => Button::Right,
        MouseButton::Middle => Button::Middle,
    }
}

/// Map a named key (as used by [`InputBackend::key_press`]) to an enigo [`Key`].
///
/// Recognizes the common control/navigation keys; any single printable
/// character falls through to [`Key::Unicode`]. Anything else is rejected with
/// [`GolemError::Input`].
fn map_key(name: &str) -> Result<Key> {
    let key = match name {
        "Enter" | "Return" => Key::Return,
        "Tab" => Key::Tab,
        "Escape" | "Esc" => Key::Escape,
        "Backspace" => Key::Backspace,
        "Space" => Key::Space,
        "Delete" | "Del" => Key::Delete,
        "Home" => Key::Home,
        "End" => Key::End,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        "ArrowUp" | "Up" => Key::UpArrow,
        "ArrowDown" | "Down" => Key::DownArrow,
        "ArrowLeft" | "Left" => Key::LeftArrow,
        "ArrowRight" | "Right" => Key::RightArrow,
        other => {
            let mut chars = other.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Key::Unicode(c),
                _ => {
                    return Err(GolemError::Input(format!("unknown key name: {other}")));
                }
            }
        }
    };
    Ok(key)
}

#[async_trait]
impl InputBackend for NativeInput {
    async fn mouse_move_abs(&self, x: i32, y: i32) -> Result<()> {
        self.dispatch(|reply| InputCmd::MoveAbs { x, y, reply }).await
    }

    async fn mouse_press(&self, button: MouseButton) -> Result<()> {
        self.dispatch(|reply| InputCmd::Button {
            button,
            direction: Press,
            reply,
        })
        .await
    }

    async fn mouse_release(&self, button: MouseButton) -> Result<()> {
        self.dispatch(|reply| InputCmd::Button {
            button,
            direction: Release,
            reply,
        })
        .await
    }

    async fn cursor_pos(&self) -> Result<(i32, i32)> {
        self.dispatch(|reply| InputCmd::CursorPos { reply }).await
    }

    async fn scroll(&self, delta_x: i32, delta_y: i32) -> Result<()> {
        self.dispatch(|reply| InputCmd::Scroll {
            dx: delta_x,
            dy: delta_y,
            reply,
        })
        .await
    }

    async fn key_char(&self, c: char) -> Result<()> {
        self.dispatch(|reply| InputCmd::Key {
            key: Key::Unicode(c),
            direction: Click,
            reply,
        })
        .await
    }

    async fn key_press(&self, key: &str) -> Result<()> {
        let mapped = map_key(key)?;
        self.dispatch(|reply| InputCmd::Key {
            key: mapped,
            direction: Click,
            reply,
        })
        .await
    }

    async fn key_down(&self, key: &str) -> Result<()> {
        let mapped = map_key(key)?;
        self.dispatch(|reply| InputCmd::Key {
            key: mapped,
            direction: Press,
            reply,
        })
        .await
    }

    async fn key_up(&self, key: &str) -> Result<()> {
        let mapped = map_key(key)?;
        self.dispatch(|reply| InputCmd::Key {
            key: mapped,
            direction: Release,
            reply,
        })
        .await
    }

    fn is_available(&self) -> bool {
        self.available.load(Ordering::Relaxed)
    }
}
