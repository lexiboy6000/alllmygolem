//! The engine: owns the browser connection, runs workflows under supervision,
//! checkpoints per step, isolates panics, and reconnects on CDP drops. Runs on
//! a background tokio runtime; talks to the GUI only through channels.
//!
//! The engine itself never runs a workflow inline — each "run" (a target plus
//! its dependencies and `run_after` follow-ups) is handed to a spawned
//! [`chain`] task so the command loop keeps processing Stop/Pause/Prompt
//! concurrently. State that outlives individual runs (settings, the input
//! backend, the browser, the shared [`Control`]/[`PromptBus`], the busy flag)
//! lives here in the loop.

mod chain;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tokio::task::JoinHandle;

use crate::backend::{BrowserBackend, InputBackend, MouseButton};
use crate::geometry::Rect;
use crate::cdp::{CdpBrowser, ConnectionConfig};
use crate::checkpoint::RunState;
use crate::context::{Control, PromptBus};
use crate::error::{GolemError, Result};
use crate::input::NativeInput;
use crate::messages::{
    CommandRx, ConnState, EngineEvent, EngineStatus, EventTx, LogLevel, UiCommand,
};
use crate::registry::WorkflowRegistry;
use crate::settings::Settings;

use chain::ChainArgs;

pub struct Engine;

impl Engine {
    /// Run the engine event loop until `Shutdown`. Returns `Ok(())` on a clean
    /// shutdown. The only error path is a fatal startup failure; routine
    /// problems are surfaced as [`EngineEvent::Error`] and the loop continues.
    pub async fn run(
        mut settings: Settings,
        registry: WorkflowRegistry,
        mut commands: CommandRx,
        events: EventTx,
    ) -> Result<()> {
        // The registry is shared, read-only, into each spawned chain task.
        let registry = Arc::new(registry);

        // Built once: the native-input backend. `NativeInput::new` returns `Ok`
        // in practice; if it ever fails we log and fall back to a no-op backend
        // so the engine still runs (CDP-strategy workflows are unaffected).
        let input: Arc<dyn InputBackend> = match NativeInput::new() {
            Ok(native) => native,
            Err(e) => {
                tracing::warn!("native input unavailable ({e}); using no-op input backend");
                let _ = events.send(EngineEvent::Log {
                    level: LogLevel::Warn,
                    message: format!("native input unavailable: {e}"),
                });
                Arc::new(NoopInput)
            }
        };

        // A no-op browser used to run browser-less workflows (e.g. the Docker/
        // Claude solve pipeline) without requiring a Chrome connection.
        let noop_browser: Arc<dyn BrowserBackend> = Arc::new(NoopBrowser);

        // Shared cooperative control + prompt routing, live for the whole loop.
        let control = Control::new();
        let prompts = PromptBus::new();
        let busy = Arc::new(AtomicBool::new(false));

        // The connected browser (None until the user presses Connect).
        let mut browser: Option<Arc<dyn BrowserBackend>> = None;
        // The currently-running chain task, if any.
        let mut current: Option<JoinHandle<()>> = None;

        // --- startup announcements ---
        let _ = events.send(EngineEvent::Workflows(registry.list()));
        let _ = events.send(EngineEvent::Status(EngineStatus::Idle));
        let _ = events.send(EngineEvent::Log {
            level: LogLevel::Info,
            message: "engine started".into(),
        });

        // Offer to resume the latest still-"running" checkpoint, if enabled.
        if settings.auto_resume {
            match RunState::latest(&settings.checkpoint_dir()) {
                Ok(Some(rs)) if rs.status == "running" => {
                    let _ = events.send(EngineEvent::ResumeAvailable {
                        run_id: rs.run_id,
                        workflow: rs.workflow,
                        step: rs.step_name,
                    });
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!("checkpoint scan failed: {e}");
                }
            }
        }

        // --- command loop ---
        while let Some(cmd) = commands.recv().await {
            match cmd {
                UiCommand::Connect => {
                    let _ = events.send(EngineEvent::Status(EngineStatus::Connecting));
                    let _ = events.send(EngineEvent::Connection(ConnState::Connecting));
                    let cfg = ConnectionConfig::from_settings(&settings);
                    match CdpBrowser::connect(cfg, events.clone()).await {
                        Ok(b) => {
                            let b: Arc<dyn BrowserBackend> = b;
                            browser = Some(b);
                            let _ = events.send(EngineEvent::Status(EngineStatus::Connected));
                        }
                        Err(e) => {
                            let _ = events.send(EngineEvent::Error(e.to_string()));
                            let _ = events
                                .send(EngineEvent::Connection(ConnState::Disconnected));
                            let _ = events.send(EngineEvent::Status(EngineStatus::Idle));
                        }
                    }
                }

                UiCommand::LaunchChrome { user_data_dir } => {
                    let cfg = ConnectionConfig::from_settings(&settings);
                    let udd = user_data_dir
                        .filter(|s| !s.trim().is_empty())
                        .or_else(|| {
                            settings
                                .chrome_user_data_dir
                                .clone()
                                .filter(|s| !s.trim().is_empty())
                        })
                        .unwrap_or_else(|| {
                            settings
                                .output_dir
                                .join("chrome-profile")
                                .to_string_lossy()
                                .into_owned()
                        });
                    // Chrome refuses to open a profile that another Chrome already has
                    // open: the new process briefly exposes a debug server, hands the
                    // request off to the running instance, then exits — so the CDP
                    // connection drops on the first command ("receiver is gone"). Detect
                    // an in-use profile via Chrome's `SingletonLock` and connect to the
                    // existing instance instead of launching a doomed duplicate.
                    let in_use = std::path::Path::new(&udd)
                        .join("SingletonLock")
                        .symlink_metadata()
                        .is_ok();
                    let ready = if in_use {
                        let _ = events.send(EngineEvent::Log {
                            level: LogLevel::Warn,
                            message: format!(
                                "profile {udd} is already open in another Chrome — Chrome can't open it twice, so connecting to the existing instance instead of launching a duplicate. If this hangs, that Chrome wasn't started with --remote-debugging-port={}; close it and relaunch from here, or pick the Golem dedicated profile.",
                                cfg.port
                            ),
                        });
                        true
                    } else {
                        match crate::cdp::launch_debug_browser(
                            cfg.port,
                            cfg.chrome_path.as_deref(),
                            &udd,
                        ) {
                            Ok(bin) => {
                                let _ = events.send(EngineEvent::Log {
                                    level: LogLevel::Info,
                                    message: format!(
                                        "launched {bin} with --remote-debugging-port={} (profile: {udd}); connecting…",
                                        cfg.port
                                    ),
                                });
                                true
                            }
                            Err(e) => {
                                let _ = events.send(EngineEvent::Error(e.to_string()));
                                let _ = events
                                    .send(EngineEvent::Connection(ConnState::Disconnected));
                                let _ = events.send(EngineEvent::Status(EngineStatus::Idle));
                                false
                            }
                        }
                    };
                    if ready {
                        let _ = events.send(EngineEvent::Status(EngineStatus::Connecting));
                        let _ = events.send(EngineEvent::Connection(ConnState::Connecting));
                        // Wait for the debug endpoint to come up, then attach.
                        let mut up = false;
                        for _ in 0..26 {
                            if crate::cdp::endpoint_reachable(&cfg).await {
                                up = true;
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(350)).await;
                        }
                        if up {
                            match CdpBrowser::connect(cfg, events.clone()).await {
                                Ok(b) => {
                                    let b: Arc<dyn BrowserBackend> = b;
                                    browser = Some(b);
                                    let _ = events
                                        .send(EngineEvent::Status(EngineStatus::Connected));
                                }
                                Err(e) => {
                                    let _ = events.send(EngineEvent::Error(e.to_string()));
                                    let _ = events
                                        .send(EngineEvent::Connection(ConnState::Disconnected));
                                    let _ = events.send(EngineEvent::Status(EngineStatus::Idle));
                                }
                            }
                        } else if in_use {
                            let _ = events.send(EngineEvent::Error(format!(
                                "profile {udd} is open in another Chrome that has no debug port on {}. Close that Chrome and relaunch from here, or pick the Golem dedicated profile.",
                                cfg.port
                            )));
                            let _ = events.send(EngineEvent::Connection(ConnState::Disconnected));
                            let _ = events.send(EngineEvent::Status(EngineStatus::Idle));
                        } else {
                            let _ = events.send(EngineEvent::Error(format!(
                                "Chrome launched but the debug endpoint on port {} never came up",
                                cfg.port
                            )));
                            let _ = events.send(EngineEvent::Connection(ConnState::Disconnected));
                            let _ = events.send(EngineEvent::Status(EngineStatus::Idle));
                        }
                    }
                }

                UiCommand::Disconnect => {
                    browser = None;
                    let _ = events.send(EngineEvent::Connection(ConnState::Disconnected));
                    let _ = events.send(EngineEvent::Status(EngineStatus::Idle));
                }

                UiCommand::Run { workflow, inputs } => {
                    if busy.load(Ordering::SeqCst) {
                        let _ = events
                            .send(EngineEvent::Error("a workflow is already running".into()));
                    } else {
                        let targets = vec![workflow];
                        match resolve_browser(browser.as_ref(), &noop_browser, &registry, &targets) {
                            Some(b) => {
                                let args = ChainArgs {
                                    registry: registry.clone(),
                                    browser: b,
                                    input: input.clone(),
                                    control: control.clone(),
                                    prompts: prompts.clone(),
                                    events: events.clone(),
                                    settings: settings.clone(),
                                    busy: busy.clone(),
                                    targets,
                                    inputs,
                                    restore: None,
                                    confirm_prereqs: true,
                                };
                                current = Some(tokio::spawn(args.run()));
                            }
                            None => {
                                let _ = events.send(EngineEvent::Error(
                                    "not connected to Chrome; press Connect".into(),
                                ));
                            }
                        }
                    }
                }

                UiCommand::RunChain { workflows, inputs } => {
                    if busy.load(Ordering::SeqCst) {
                        let _ = events
                            .send(EngineEvent::Error("a workflow is already running".into()));
                    } else {
                        let targets = workflows;
                        match resolve_browser(browser.as_ref(), &noop_browser, &registry, &targets) {
                            Some(b) => {
                                let args = ChainArgs {
                                    registry: registry.clone(),
                                    browser: b,
                                    input: input.clone(),
                                    control: control.clone(),
                                    prompts: prompts.clone(),
                                    events: events.clone(),
                                    settings: settings.clone(),
                                    busy: busy.clone(),
                                    targets,
                                    inputs,
                                    restore: None,
                                    confirm_prereqs: false,
                                };
                                current = Some(tokio::spawn(args.run()));
                            }
                            None => {
                                let _ = events.send(EngineEvent::Error(
                                    "not connected to Chrome; press Connect".into(),
                                ));
                            }
                        }
                    }
                }

                UiCommand::Stop => {
                    // Cooperative first: wake any paused/await points and unblock
                    // a pending prompt so the workflow can unwind cleanly.
                    control.request_stop();
                    prompts.cancel_all();
                    // Authoritative: abort the chain task so Stop is immediate even
                    // if the workflow is mid-poll (e.g. wait_for_selector) where it
                    // isn't checking the flag. Then reset state ourselves, since the
                    // chain's own cleanup won't run after an abort.
                    if busy.load(Ordering::SeqCst) {
                        if let Some(handle) = current.take() {
                            handle.abort();
                            let _ = handle.await;
                        }
                        busy.store(false, Ordering::SeqCst);
                        let _ = events.send(EngineEvent::Status(EngineStatus::Stopped));
                        let _ = events.send(EngineEvent::ChainFinished);
                    }
                }

                UiCommand::Pause => {
                    control.request_pause();
                    let _ = events.send(EngineEvent::Status(EngineStatus::Paused));
                }

                UiCommand::Resume => {
                    control.resume();
                }

                UiCommand::PromptResponse { id, value } => {
                    prompts.resolve(id, value);
                }

                UiCommand::UpdateSettings(boxed) => {
                    settings = *boxed;
                    tracing::info!("settings updated; new values apply to the next run");
                    let _ = events.send(EngineEvent::Log {
                        level: LogLevel::Info,
                        message: "settings updated (applied on next run)".into(),
                    });
                }

                UiCommand::ResumeCheckpoint { run_id } => {
                    if busy.load(Ordering::SeqCst) {
                        let _ = events
                            .send(EngineEvent::Error("a workflow is already running".into()));
                    } else {
                        let path = RunState::file_path(&settings.checkpoint_dir(), &run_id);
                        match RunState::load(&path) {
                            Ok(rs) => {
                                let targets = vec![rs.workflow.clone()];
                                match resolve_browser(
                                    browser.as_ref(),
                                    &noop_browser,
                                    &registry,
                                    &targets,
                                ) {
                                    Some(b) => {
                                        let args = ChainArgs {
                                            registry: registry.clone(),
                                            browser: b,
                                            input: input.clone(),
                                            control: control.clone(),
                                            prompts: prompts.clone(),
                                            events: events.clone(),
                                            settings: settings.clone(),
                                            busy: busy.clone(),
                                            targets,
                                            inputs: rs.inputs.clone(),
                                            restore: Some(rs),
                                            confirm_prereqs: true,
                                        };
                                        current = Some(tokio::spawn(args.run()));
                                    }
                                    None => {
                                        let _ = events.send(EngineEvent::Error(
                                            "not connected to Chrome; press Connect".into(),
                                        ));
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = events.send(EngineEvent::Error(format!(
                                    "could not load checkpoint '{run_id}': {e}"
                                )));
                            }
                        }
                    }
                }

                UiCommand::Shutdown => {
                    control.request_stop();
                    prompts.cancel_all();
                    break;
                }
            }
        }

        // Let any in-flight chain observe the stop request and unwind cleanly.
        if let Some(handle) = current.take() {
            let _ = handle.await;
        }

        Ok(())
    }
}

/// Fallback input backend used only when the native backend cannot be built.
/// Every operation reports unavailability rather than silently doing nothing,
/// so a workflow that relies on native input fails fast with a clear error.
struct NoopInput;

#[async_trait]
impl InputBackend for NoopInput {
    async fn mouse_move_abs(&self, _x: i32, _y: i32) -> Result<()> {
        Err(unavailable())
    }
    async fn mouse_press(&self, _button: MouseButton) -> Result<()> {
        Err(unavailable())
    }
    async fn mouse_release(&self, _button: MouseButton) -> Result<()> {
        Err(unavailable())
    }
    async fn cursor_pos(&self) -> Result<(i32, i32)> {
        Err(unavailable())
    }
    async fn scroll(&self, _delta_x: i32, _delta_y: i32) -> Result<()> {
        Err(unavailable())
    }
    async fn key_char(&self, _c: char) -> Result<()> {
        Err(unavailable())
    }
    async fn key_press(&self, _key: &str) -> Result<()> {
        Err(unavailable())
    }
    async fn key_down(&self, _key: &str) -> Result<()> {
        Err(unavailable())
    }
    async fn key_up(&self, _key: &str) -> Result<()> {
        Err(unavailable())
    }
    fn is_available(&self) -> bool {
        false
    }
}

fn unavailable() -> GolemError {
    GolemError::Input("native input backend unavailable".into())
}

/// Pick the browser backend for a run: the connected one if present, otherwise a
/// no-op backend when every workflow in the chain declares `requires_browser() ==
/// false`. Returns `None` when a real connection is required but absent.
fn resolve_browser(
    connected: Option<&Arc<dyn BrowserBackend>>,
    noop: &Arc<dyn BrowserBackend>,
    registry: &WorkflowRegistry,
    targets: &[String],
) -> Option<Arc<dyn BrowserBackend>> {
    if let Some(b) = connected {
        return Some(b.clone());
    }
    if chain_browserless(registry, targets) {
        return Some(noop.clone());
    }
    None
}

/// Whether every workflow reachable from `targets` (deps included) is browser-less.
fn chain_browserless(registry: &WorkflowRegistry, targets: &[String]) -> bool {
    !targets.is_empty()
        && targets.iter().all(|t| match registry.resolve_order(t) {
            Ok(order) => order.iter().all(|n| {
                registry
                    .get(n)
                    .map(|w| !w.requires_browser())
                    .unwrap_or(false)
            }),
            Err(_) => false,
        })
}

/// A browser backend that errors on every call — used to run browser-less
/// workflows without a Chrome connection.
struct NoopBrowser;

fn no_browser() -> GolemError {
    GolemError::Browser("this workflow does not use the browser".into())
}

#[async_trait]
impl BrowserBackend for NoopBrowser {
    async fn navigate(&self, _url: &str) -> Result<()> {
        Err(no_browser())
    }
    async fn current_url(&self) -> Result<String> {
        Err(no_browser())
    }
    async fn wait_for_selector(&self, _selector: &str, _timeout: Duration) -> Result<()> {
        Err(no_browser())
    }
    async fn query_exists(&self, _selector: &str) -> Result<bool> {
        Err(no_browser())
    }
    async fn get_attribute(&self, _selector: &str, _name: &str) -> Result<Option<String>> {
        Err(no_browser())
    }
    async fn get_text(&self, _selector: &str) -> Result<Option<String>> {
        Err(no_browser())
    }
    async fn get_rect(&self, _selector: &str) -> Result<Option<Rect>> {
        Err(no_browser())
    }
    async fn eval(&self, _js: &str) -> Result<serde_json::Value> {
        Err(no_browser())
    }
    async fn focus(&self, _selector: &str) -> Result<()> {
        Err(no_browser())
    }
    async fn mouse_move(&self, _x: f64, _y: f64) -> Result<()> {
        Err(no_browser())
    }
    async fn mouse_press(&self, _button: MouseButton, _x: f64, _y: f64) -> Result<()> {
        Err(no_browser())
    }
    async fn mouse_release(&self, _button: MouseButton, _x: f64, _y: f64) -> Result<()> {
        Err(no_browser())
    }
    async fn mouse_wheel(&self, _x: f64, _y: f64, _dx: f64, _dy: f64) -> Result<()> {
        Err(no_browser())
    }
    async fn key_char(&self, _c: char) -> Result<()> {
        Err(no_browser())
    }
    async fn key_type(&self, _c: char) -> Result<()> {
        Err(no_browser())
    }
    async fn key_press(&self, _key: &str) -> Result<()> {
        Err(no_browser())
    }
    async fn viewport_size(&self) -> Result<(f64, f64)> {
        Err(no_browser())
    }
    async fn set_download_dir(&self, _dir: &Path) -> Result<()> {
        Err(no_browser())
    }
    async fn cookies_header(&self) -> Result<String> {
        Err(no_browser())
    }
    async fn user_agent(&self) -> Result<String> {
        Err(no_browser())
    }
}
