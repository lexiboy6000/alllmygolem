//! `WorkflowCtx` — the entire surface a workflow author touches. It wraps the
//! browser + native backends with human-like motion, cooperative stop/pause,
//! user prompts, progress reporting and per-step checkpointing.
//!
//! Adding a workflow is meant to be trivial: `use crate::prelude::*;`, then call
//! `ctx.navigate`, `ctx.click`, `ctx.type_into`, `ctx.wait_for`, `ctx.attr`,
//! `ctx.confirm`, `ctx.download`, `ctx.step(...)`, etc. The raw `ctx.browser`,
//! `ctx.input` and `ctx.eval` escape hatches expose *anything* CDP or native
//! input can do.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{Notify, oneshot};
use uuid::Uuid;

use crate::backend::{BrowserBackend, InputBackend, MouseButton};
use crate::checkpoint::RunState;
use crate::error::{GolemError, Result};
use crate::geometry::Point;
use rand::RngExt;

use crate::humanize;
use crate::messages::{
    CommandTx, EngineEvent, EngineStatus, EventTx, LogLevel, PromptKind, PromptRequest,
    PromptResponse,
};
use crate::settings::{InputStrategy, Settings};

/// Result of running a subprocess via [`WorkflowCtx::run`].
#[derive(Clone, Debug)]
pub struct CommandOutput {
    /// Exit code, or `None` if the process was terminated by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
    /// stdout and stderr joined (whichever are non-empty).
    pub fn combined(&self) -> String {
        match (self.stdout.trim().is_empty(), self.stderr.trim().is_empty()) {
            (false, false) => format!("{}\n{}", self.stdout, self.stderr),
            (false, true) => self.stdout.clone(),
            (true, false) => self.stderr.clone(),
            (true, true) => String::new(),
        }
    }
}

/// Cooperative stop/pause shared between the engine and a running workflow.
/// Workflows hit `await` points frequently (every `ctx` action calls `guard`),
/// so Stop is responsive without aborting threads.
pub struct Control {
    stop: AtomicBool,
    pause: AtomicBool,
    notify: Notify,
}

impl Control {
    pub fn new() -> Arc<Control> {
        Arc::new(Control {
            stop: AtomicBool::new(false),
            pause: AtomicBool::new(false),
            notify: Notify::new(),
        })
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
    pub fn request_pause(&self) {
        self.pause.store(true, Ordering::SeqCst);
    }
    pub fn resume(&self) {
        self.pause.store(false, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
    /// Clear flags for a fresh run.
    pub fn reset(&self) {
        self.stop.store(false, Ordering::SeqCst);
        self.pause.store(false, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
    pub fn is_paused(&self) -> bool {
        self.pause.load(Ordering::SeqCst)
    }

    /// Resolve once Stop has been requested. Used to race against a running
    /// subprocess so we can kill it promptly.
    pub async fn wait_until_stopped(&self) {
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return;
            }
            let notified = self.notify.notified();
            if self.stop.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }

    /// The cooperative checkpoint: returns `Err(StoppedByUser)` if Stop was
    /// requested, otherwise parks while paused, then returns `Ok`.
    pub async fn wait_if_paused(&self) -> Result<()> {
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return Err(GolemError::StoppedByUser);
            }
            if !self.pause.load(Ordering::SeqCst) {
                return Ok(());
            }
            // Park until resume()/stop() notifies, then re-check.
            self.notify.notified().await;
        }
    }
}

/// Routes user prompt responses from the engine back to the awaiting workflow.
pub struct PromptBus {
    pending: Mutex<BTreeMap<Uuid, oneshot::Sender<PromptResponse>>>,
}

impl PromptBus {
    pub fn new() -> Arc<PromptBus> {
        Arc::new(PromptBus {
            pending: Mutex::new(BTreeMap::new()),
        })
    }

    /// Register interest in a prompt id; the returned receiver resolves when the
    /// engine calls [`PromptBus::resolve`].
    pub fn register(&self, id: Uuid) -> oneshot::Receiver<PromptResponse> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id, tx);
        rx
    }

    /// Deliver a response (called by the engine on `UiCommand::PromptResponse`).
    pub fn resolve(&self, id: Uuid, response: PromptResponse) {
        if let Some(tx) = self.pending.lock().remove(&id) {
            let _ = tx.send(response);
        }
    }

    /// Cancel every pending prompt (called on Stop). Dropping the senders makes
    /// the awaiting receivers error out, which the workflow maps to a stop.
    pub fn cancel_all(&self) {
        self.pending.lock().clear();
    }
}

impl Default for PromptBus {
    fn default() -> Self {
        PromptBus {
            pending: Mutex::new(BTreeMap::new()),
        }
    }
}

/// Whether this process is running under a Wayland compositor. Golem and the
/// browser it drives share a session, so the answer applies to both.
///
/// It matters because a Wayland client is never told where its own window sits
/// on screen -- there is no protocol for it -- so `window.screenX`/`screenY`
/// are a constant 0 and the real OS cursor cannot be aimed at a page element.
/// enigo can't read the pointer position under Wayland either, so native moves
/// fall back to [`WorkflowCtx::last_native_mouse`] for their start point, and
/// the window position is obtained from the compositor instead -- see
/// [`compositor_browser_window_pos`].
fn is_wayland_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t.eq_ignore_ascii_case("wayland"))
}

/// Everything passed to `Workflow::run`.
pub struct WorkflowCtx {
    /// Raw browser/CDP backend — escape hatch for anything not wrapped below.
    pub browser: Arc<dyn BrowserBackend>,
    /// Raw native-input backend — for OS dialogs and the Native/Hybrid strategy.
    pub input: Arc<dyn InputBackend>,
    pub control: Arc<Control>,
    pub prompts: Arc<PromptBus>,
    pub events: EventTx,
    pub settings: Settings,
    pub run_id: String,
    pub workflow: String,

    store: BTreeMap<String, Value>,
    inputs: BTreeMap<String, String>,
    last_mouse: Point,
    /// Set once the window turns out to be unmappable to screen pixels (the
    /// Wayland case — see [`viewport_pointer_map`](Self::viewport_pointer_map)).
    /// It cannot become mappable later in a run, so cache it: without this,
    /// every native-cursor click re-probes and logs the same warning.
    pointer_unmappable: bool,
    /// Whether the "browser window isn't on the displayed workspace" warning
    /// has already been given. Cleared as soon as it comes back into view, so
    /// the warning marks each time it goes away rather than every click.
    pointer_offscreen_noted: bool,
    /// Last screen point we drove the REAL OS cursor to. Wayland has no
    /// protocol for reading the cursor position, so this is the fallback
    /// start point for native moves when `cursor_pos()` fails.
    last_native_mouse: Option<Point>,
    step_index: usize,
    http: reqwest::Client,
    /// Sender back into the engine's command queue (see [`queue_chain`](Self::queue_chain)).
    commands: CommandTx,
    /// The ordered target list of the chain this workflow is running in
    /// (recorded by the chain runner via [`set_chain_targets`](Self::set_chain_targets)).
    /// Lets a workflow re-queue its own whole run -- see [`chain_targets`](Self::chain_targets).
    chain_targets: Vec<String>,
}

impl WorkflowCtx {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        browser: Arc<dyn BrowserBackend>,
        input: Arc<dyn InputBackend>,
        control: Arc<Control>,
        prompts: Arc<PromptBus>,
        events: EventTx,
        settings: Settings,
        commands: CommandTx,
        run_id: String,
        workflow: String,
        inputs: BTreeMap<String, String>,
    ) -> Result<WorkflowCtx> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| GolemError::Other(format!("http client: {e}")))?;
        Ok(WorkflowCtx {
            browser,
            input,
            control,
            prompts,
            events,
            settings,
            run_id,
            workflow,
            store: BTreeMap::new(),
            inputs,
            last_mouse: Point::ZERO,
            pointer_unmappable: false,
            pointer_offscreen_noted: false,
            last_native_mouse: None,
            step_index: 0,
            http,
            commands,
            chain_targets: Vec::new(),
        })
    }

    /// Queue another workflow chain to start as soon as the CURRENT chain
    /// finishes (the engine holds exactly one queued chain while busy; Stop
    /// discards it). Used by the pipeline's final workflow to hand off to the
    /// next round. The queued chain runs its prerequisites without a confirm
    /// prompt, exactly like a programmatic `RunChain`.
    pub fn queue_chain(&self, workflows: Vec<String>, inputs: BTreeMap<String, String>) {
        let _ = self
            .commands
            .send(crate::messages::UiCommand::RunChain { workflows, inputs });
    }

    // ----- reporting -------------------------------------------------------

    fn emit(&self, event: EngineEvent) {
        // GUI may be gone; never fail a workflow because nobody's listening.
        let _ = self.events.send(event);
    }

    /// A user-facing output line (shown in the output pane).
    pub fn output(&self, msg: impl Into<String>) {
        let msg = msg.into();
        tracing::info!(target: "workflow", "{msg}");
        self.emit(EngineEvent::Output(msg));
    }
    pub fn info(&self, msg: impl Into<String>) {
        let msg = msg.into();
        tracing::info!(target: "workflow", "{msg}");
        self.emit(EngineEvent::Log {
            level: LogLevel::Info,
            message: msg,
        });
    }
    pub fn warn(&self, msg: impl Into<String>) {
        let msg = msg.into();
        tracing::warn!(target: "workflow", "{msg}");
        self.emit(EngineEvent::Log {
            level: LogLevel::Warn,
            message: msg,
        });
    }
    pub fn progress(&self, fraction: Option<f32>, label: impl Into<String>) {
        self.emit(EngineEvent::Progress {
            fraction,
            label: label.into(),
        });
    }
    fn set_step(&self, step: impl Into<String>) {
        self.emit(EngineEvent::Status(EngineStatus::Running {
            workflow: self.workflow.clone(),
            step: step.into(),
        }));
    }

    /// Update the live "Running" sub-status shown in the GUI (and overlay)
    /// WITHOUT advancing the step counter or checkpointing. Use for fine-grained
    /// activity within a step (e.g. "typing line 12/81", "thinking…", "saving").
    pub fn note_status(&self, step: impl Into<String>) {
        self.set_step(step);
    }

    // ----- control / stop / pause -----------------------------------------

    /// Cooperative cancellation point. Every wrapped action calls this; call it
    /// yourself inside long loops too.
    pub async fn guard(&self) -> Result<()> {
        self.control.wait_if_paused().await
    }

    /// Mark a logical step boundary: guard, advance the counter, update status,
    /// and checkpoint to disk. Call at the start of each step in a workflow.
    pub async fn step(&mut self, name: impl Into<String>) -> Result<()> {
        self.guard().await?;
        let name = name.into();
        self.step_index += 1;
        self.set_step(name.clone());
        self.progress(None, name.clone());
        self.info(format!("step: {name}"));
        self.persist_checkpoint(&name, "running")?;
        Ok(())
    }

    fn persist_checkpoint(&self, step_name: &str, status: &str) -> Result<()> {
        let rs = RunState {
            run_id: self.run_id.clone(),
            workflow: self.workflow.clone(),
            step_index: self.step_index,
            step_name: step_name.to_string(),
            store: self.store.clone(),
            inputs: self.inputs.clone(),
            updated: chrono::Utc::now().to_rfc3339(),
            status: status.to_string(),
        };
        rs.save(&self.settings.checkpoint_dir())
    }

    // ----- user prompts ----------------------------------------------------

    async fn ask(&self, message: String, kind: PromptKind) -> Result<PromptResponse> {
        self.guard().await?;
        let id = Uuid::new_v4();
        let rx = self.prompts.register(id);
        self.emit(EngineEvent::Prompt(PromptRequest { id, message, kind }));
        match rx.await {
            Ok(resp) => Ok(resp),
            Err(_) => {
                if self.control.is_stopped() {
                    Err(GolemError::StoppedByUser)
                } else {
                    Err(GolemError::Prompt("prompt was cancelled".into()))
                }
            }
        }
    }

    /// Ask for free text, returning the entered value.
    pub async fn prompt_text(
        &self,
        message: impl Into<String>,
        default: impl Into<String>,
    ) -> Result<String> {
        match self
            .ask(
                message.into(),
                PromptKind::Text {
                    default: default.into(),
                },
            )
            .await?
        {
            PromptResponse::Text(s) => Ok(s),
            PromptResponse::Dismiss => Err(GolemError::Prompt("input cancelled".into())),
            _ => Err(GolemError::Prompt("unexpected prompt response".into())),
        }
    }

    /// Ask a yes/no question.
    pub async fn confirm(&self, message: impl Into<String>) -> Result<bool> {
        match self.ask(message.into(), PromptKind::Confirm).await? {
            PromptResponse::Bool(b) => Ok(b),
            PromptResponse::Dismiss => Ok(false),
            _ => Err(GolemError::Prompt("unexpected prompt response".into())),
        }
    }

    /// Offer a choice; returns the chosen index.
    pub async fn choose(
        &self,
        message: impl Into<String>,
        options: Vec<String>,
    ) -> Result<usize> {
        match self
            .ask(message.into(), PromptKind::Choice { options })
            .await?
        {
            PromptResponse::Choice(i) => Ok(i),
            _ => Err(GolemError::Prompt("unexpected prompt response".into())),
        }
    }

    /// Show a warning and wait for acknowledgement (does not itself stop).
    pub async fn warn_user(&self, message: impl Into<String>) -> Result<()> {
        let _ = self.ask(message.into(), PromptKind::Info).await?;
        Ok(())
    }

    /// Ask the GUI to persist a new default value for a workflow input (e.g.
    /// "make this git SHA the default"). Applies to future runs.
    pub fn set_default_input(&self, workflow: &str, key: &str, value: &str) {
        self.emit(EngineEvent::SetWorkflowInput {
            workflow: workflow.to_string(),
            key: key.to_string(),
            value: value.to_string(),
        });
    }

    /// Build a halt error (the spec's "STOP and warn user") without prompting.
    pub fn halt(&self, message: impl Into<String>) -> GolemError {
        let m = message.into();
        tracing::warn!(target: "workflow", "halt: {m}");
        self.emit(EngineEvent::Error(m.clone()));
        GolemError::Halted(m)
    }

    /// The spec's "STOP and prompt user": surface the message, wait for the user
    /// to acknowledge, then halt the chain. Use as `return Err(ctx.stop_and_warn(..).await);`
    pub async fn stop_and_warn(&self, message: impl Into<String>) -> GolemError {
        let m = message.into();
        let _ = self.warn_user(m.clone()).await;
        GolemError::Halted(m)
    }

    // ----- store / inputs --------------------------------------------------

    pub fn set<T: Serialize>(&mut self, key: impl Into<String>, value: T) -> Result<()> {
        let v = serde_json::to_value(value)?;
        self.store.insert(key.into(), v);
        Ok(())
    }
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.store.get(key)
    }
    pub fn get_str(&self, key: &str) -> Option<String> {
        self.store.get(key).map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
    }
    pub fn input(&self, key: &str) -> Option<&str> {
        self.inputs.get(key).map(|s| s.as_str())
    }
    pub fn require_input(&self, key: &str) -> Result<String> {
        self.inputs
            .get(key)
            .cloned()
            .ok_or_else(|| GolemError::Other(format!("missing required input '{key}'")))
    }
    pub fn store_snapshot(&self) -> BTreeMap<String, Value> {
        self.store.clone()
    }
    /// A copy of every input this run was started with, for re-queueing the
    /// chain with identical inputs (see [`chain_targets`](Self::chain_targets)).
    pub fn inputs_snapshot(&self) -> BTreeMap<String, String> {
        self.inputs.clone()
    }
    /// Record the chain's original target list (called by the chain runner
    /// right after building this context).
    pub fn set_chain_targets(&mut self, targets: Vec<String>) {
        self.chain_targets = targets;
    }
    /// The ordered target list of the chain this workflow is part of.
    /// Re-queueing it via [`queue_chain`](Self::queue_chain) restarts the
    /// whole run from its first dependency (workflow 1, for the task
    /// pipeline) -- the skip-and-restart recovery in the download workflows
    /// uses exactly that. Falls back to just this workflow's own name if the
    /// runner never recorded targets.
    pub fn chain_targets(&self) -> Vec<String> {
        if self.chain_targets.is_empty() {
            vec![self.workflow.clone()]
        } else {
            self.chain_targets.clone()
        }
    }
    /// Restore store + step index when resuming from a checkpoint.
    pub fn restore(&mut self, state: &RunState) {
        self.store = state.store.clone();
        self.step_index = state.step_index;
    }

    // ----- browser conveniences -------------------------------------------

    pub async fn navigate(&self, url: &str) -> Result<()> {
        self.guard().await?;
        self.info(format!("navigate -> {url}"));
        self.browser.navigate(url).await
    }
    pub async fn current_url(&self) -> Result<String> {
        self.guard().await?;
        self.browser.current_url().await
    }
    /// Switch the controlled target to the NEWEST tab/popup whose URL contains
    /// `url_substring` but NOT `exclude` (pass `""` for none — use it to skip a
    /// stale/expired session tab). Polls up to `timeout`. `true` if it switched.
    pub async fn switch_to_target(
        &self,
        url_substring: &str,
        exclude: &str,
        timeout: Duration,
    ) -> Result<bool> {
        self.guard().await?;
        self.info(format!("switching to target containing '{url_substring}'"));
        self.browser.switch_to_target(url_substring, exclude, timeout).await
    }
    /// Close every OTHER tab whose URL contains `url_substring`, keeping the one
    /// we're driving. Returns how many were closed. Use to clear duplicate Vagon
    /// desktops so two streams don't fight over the same workstation.
    pub async fn close_other_targets(&self, url_substring: &str) -> Result<usize> {
        self.guard().await?;
        let n = self.browser.close_other_targets(url_substring).await?;
        if n > 0 {
            self.info(format!("closed {n} duplicate tab(s) matching '{url_substring}'"));
        }
        Ok(n)
    }
    /// Bring the controlled tab to the foreground (needed for some streams to
    /// capture keyboard input).
    pub async fn bring_to_front(&self) -> Result<()> {
        self.guard().await?;
        self.browser.bring_to_front().await
    }
    /// Capture a PNG screenshot of the controlled page and write it to `path`.
    pub async fn save_screenshot(&self, path: &std::path::Path) -> Result<()> {
        self.guard().await?;
        let bytes = self.browser.screenshot().await?;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(path, &bytes)
            .map_err(|e| GolemError::Io(format!("write screenshot {}: {e}", path.display())))?;
        self.info(format!("saved screenshot -> {}", path.display()));
        Ok(())
    }
    pub async fn wait_for(&self, selector: &str, timeout: Duration) -> Result<()> {
        self.guard().await?;
        self.browser.wait_for_selector(selector, timeout).await
    }
    /// Wait using the configured default timeout.
    pub async fn wait_for_default(&self, selector: &str) -> Result<()> {
        let t = Duration::from_millis(self.settings.default_wait_timeout_ms);
        self.wait_for(selector, t).await
    }
    pub async fn exists(&self, selector: &str) -> Result<bool> {
        self.guard().await?;
        self.browser.query_exists(selector).await
    }
    pub async fn attr(&self, selector: &str, name: &str) -> Result<Option<String>> {
        self.guard().await?;
        self.browser.get_attribute(selector, name).await
    }
    pub async fn text(&self, selector: &str) -> Result<Option<String>> {
        self.guard().await?;
        self.browser.get_text(selector).await
    }
    pub async fn eval(&self, js: &str) -> Result<Value> {
        self.guard().await?;
        self.browser.eval(js).await
    }

    // ----- human-like mouse / keyboard ------------------------------------

    fn cfg(&self) -> crate::humanize::HumanizeConfig {
        self.settings.humanize
    }
    fn use_native_pointer(&self) -> bool {
        matches!(
            self.settings.input_strategy,
            InputStrategy::Native | InputStrategy::Hybrid
        )
    }
    fn use_native_keyboard(&self) -> bool {
        matches!(self.settings.input_strategy, InputStrategy::Native)
    }

    async fn raw_move(&self, p: Point) -> Result<()> {
        if self.use_native_pointer() {
            self.input.mouse_move_abs(p.x.round() as i32, p.y.round() as i32).await
        } else {
            self.browser.mouse_move(p.x, p.y).await
        }
    }
    async fn raw_press(&self, button: MouseButton, p: Point) -> Result<()> {
        if self.use_native_pointer() {
            self.input.mouse_press(button).await
        } else {
            self.browser.mouse_press(button, p.x, p.y).await
        }
    }
    async fn raw_release(&self, button: MouseButton, p: Point) -> Result<()> {
        if self.use_native_pointer() {
            self.input.mouse_release(button).await
        } else {
            self.browser.mouse_release(button, p.x, p.y).await
        }
    }

    /// Move the cursor to `target` along a human path. Updates the tracked
    /// cursor position. Cooperatively cancellable between points.
    pub async fn move_to(&mut self, target: Point) -> Result<()> {
        let cfg = self.cfg();
        let path = {
            let mut rng = rand::rng();
            humanize::mouse_path(self.last_mouse, target, &cfg, &mut rng)
        };
        for s in path {
            self.guard().await?;
            self.raw_move(s.point).await?;
            if !s.delay.is_zero() {
                tokio::time::sleep(s.delay).await;
            }
        }
        self.last_mouse = target;
        Ok(())
    }

    async fn press_release_at(&mut self, button: MouseButton, p: Point) -> Result<()> {
        let (settle, hold) = {
            let mut rng = rand::rng();
            let cfg = self.cfg();
            (
                humanize::pre_click_settle(&cfg, &mut rng),
                humanize::click_hold(&cfg, &mut rng),
            )
        };
        tokio::time::sleep(settle).await;
        self.guard().await?;
        self.raw_press(button, p).await?;
        tokio::time::sleep(hold).await;
        self.raw_release(button, p).await?;
        self.last_mouse = p;
        Ok(())
    }

    /// Human click on the first element matching `selector` (scrolls into view,
    /// arcs the cursor to a natural point inside it, then clicks).
    ///
    /// Robust to slow / SPA pages: it first WAITS for the selector to appear
    /// (polling up to the configured default timeout) instead of failing on the
    /// first miss. If the element is already present this returns immediately.
    pub async fn click(&mut self, selector: &str) -> Result<()> {
        self.guard().await?;
        let timeout = Duration::from_millis(self.settings.default_wait_timeout_ms);
        // Ignore the wait's own error: if it times out, get_rect below produces
        // the canonical NotFound(selector) error.
        let _ = self.browser.wait_for_selector(selector, timeout).await;
        let rect = self
            .browser
            .get_rect(selector)
            .await?
            .ok_or_else(|| GolemError::NotFound(selector.to_string()))?;
        let target = {
            let mut rng = rand::rng();
            rect.humanlike_point(&mut rng)
        };
        self.info(format!("click {selector}"));
        self.move_to(target).await?;
        self.press_release_at(MouseButton::Left, target).await
    }

    /// Human click at explicit viewport coordinates — use this for the
    /// remote-desktop iframe / canvas where there is no queryable DOM.
    ///
    /// Honours `input_strategy`, so under `Native`/`Hybrid` this drives the
    /// REAL cursor. That makes it the wrong thing to fall back to when the
    /// real cursor is the part that's broken — use [`click_at_cdp`] there.
    pub async fn click_at(&mut self, x: f64, y: f64) -> Result<()> {
        self.info(format!("click @ ({x:.0},{y:.0})"));
        self.move_to(Point::new(x, y)).await?;
        self.press_release_at(MouseButton::Left, Point::new(x, y)).await
    }

    /// Human click at viewport coordinates dispatched through CDP `Input.*`,
    /// regardless of the configured input strategy.
    ///
    /// This is what every "fall back to a CDP click" path must call.
    /// [`click_at`] is NOT that: it is strategy-selected, so under
    /// `Native`/`Hybrid` it hands `raw_move` the *viewport* coordinates it was
    /// given and `raw_move` passes them to `mouse_move_abs` as *screen*
    /// coordinates — no window offset added. That aims the real cursor at
    /// (viewport x, viewport y) on the raw desktop and clicks whatever happens
    /// to be there, which is exactly the mis-aim `viewport_screen_offset`
    /// refuses to guess at. Going through CDP sidesteps the mapping entirely:
    /// viewport coordinates are the only thing `Input.dispatchMouseEvent`
    /// speaks, and it needs no window focus.
    pub async fn click_at_cdp(&mut self, x: f64, y: f64) -> Result<()> {
        let target = Point::new(x, y);
        self.info(format!("cdp click @ ({x:.0},{y:.0})"));
        let cfg = self.cfg();
        let path = {
            let mut rng = rand::rng();
            humanize::mouse_path(self.last_mouse, target, &cfg, &mut rng)
        };
        for s in path {
            self.guard().await?;
            self.browser.mouse_move(s.point.x, s.point.y).await?;
            if !s.delay.is_zero() {
                tokio::time::sleep(s.delay).await;
            }
        }
        let (settle, hold) = {
            let mut rng = rand::rng();
            (
                humanize::pre_click_settle(&cfg, &mut rng),
                humanize::click_hold(&cfg, &mut rng),
            )
        };
        tokio::time::sleep(settle).await;
        self.guard().await?;
        self.browser.mouse_press(MouseButton::Left, x, y).await?;
        tokio::time::sleep(hold).await;
        self.browser.mouse_release(MouseButton::Left, x, y).await?;
        self.last_mouse = target;
        Ok(())
    }

    /// How to aim the REAL OS cursor at a page element:
    /// `screen = origin + css * css_to_screen`.
    ///
    /// Off Wayland this is derived from the window's own geometry: side
    /// borders are `(outerWidth - innerWidth) / 2` (zero on macOS), everything
    /// above the viewport (title bar, tab strip, URL bar) is the remainder of
    /// `outerHeight - innerHeight`, and `screenX/Y` says where the window is.
    /// Both `screenX/Y` and enigo speak logical (not physical-Retina) pixels,
    /// so no DPR scaling is needed there — but it does assume 100% page zoom
    /// and no docked devtools.
    ///
    /// Under a Wayland compositor (Hyprland, GNOME, Sway, ...) none of that
    /// holds, and the failure is silent rather than loud:
    ///
    /// - a Wayland client is never told where its own window sits, so
    ///   `screenX`/`screenY` are a constant 0;
    /// - with client-side decorations `outerHeight == innerHeight`, so the tab
    ///   strip and URL bar measure as zero height — the naive arithmetic puts
    ///   the viewport's origin at the window's top-left corner, ~87px too high
    ///   on a stock Chromium;
    /// - `outerWidth/Height` are window pixels while `innerWidth/Height` are
    ///   CSS pixels, so at any page zoom but 100% subtracting one from the
    ///   other mixes units and invents a frame that isn't there.
    ///
    /// So on Hyprland the window rectangle comes from the compositor
    /// ([`compositor_browser_window`]) and the viewport is measured inside it:
    /// `innerWidth/Height` scaled by the page zoom gives the viewport in the
    /// compositor's own pixels, and whatever space is left in the window is
    /// the browser frame. Page zoom is recoverable because
    /// `devicePixelRatio == monitor scale * zoom`, and the monitor's scale is
    /// something the compositor will tell us.
    ///
    /// Anywhere the mapping can't be derived this fails instead of guessing,
    /// so the caller falls back to the CDP click, which works purely in
    /// viewport coordinates and is unaffected by all of the above.
    async fn viewport_pointer_map(&self) -> Result<PointerMap> {
        let v = self
            .browser
            .eval(
                "(function(){ var side = (window.outerWidth - window.innerWidth) / 2; \
                 var chrome = window.outerHeight - window.innerHeight; \
                 return { wx: window.screenX, wy: window.screenY, \
                          iw: window.innerWidth, ih: window.innerHeight, \
                          dpr: window.devicePixelRatio, \
                          sx: window.screenX + side, \
                          sy: window.screenY + chrome - side, \
                          positioned: chrome > 0 || window.screenX !== 0 || window.screenY !== 0 }; })()",
            )
            .await?;
        let num = |k: &str| v.get(k).and_then(Value::as_f64);
        let (Some(sx), Some(sy)) = (num("sx"), num("sy")) else {
            return Err(GolemError::Other(
                "couldn't read the window's screen position".into(),
            ));
        };
        // A native-Wayland browser can't know its own screen position --
        // `screenX/Y` report 0 -- so nothing it says about where the viewport
        // is can be trusted. Measure the viewport inside the compositor's
        // window rectangle instead.
        let screen_pos_unknown = num("wx") == Some(0.0) && num("wy") == Some(0.0);
        if screen_pos_unknown
            && is_wayland_session()
            && let (Some(iw), Some(ih)) = (num("iw"), num("ih"))
            && let Some(win) = compositor_browser_window().await
            && let Some(mon) = compositor_monitor(win.monitor).await
        {
            // devicePixelRatio = monitor scale * page zoom, so this is the
            // page zoom -- the number of compositor pixels one CSS pixel
            // spans. 1.0 at 100% zoom, and the whole reason the old
            // outer-minus-inner arithmetic mis-aimed every click on a zoomed
            // page.
            let zoom = num("dpr").map_or(1.0, |dpr| dpr / mon.scale);
            return viewport_map_in_window(&win, iw, ih, zoom).ok_or_else(|| {
                GolemError::Other(format!(
                    "the {:.0}x{:.0} viewport doesn't fit the compositor's {:.0}x{:.0} \
                     window rectangle at zoom {zoom:.2} -- viewport coordinates cannot \
                     be mapped to screen pixels",
                    iw * zoom,
                    ih * zoom,
                    win.w,
                    win.h
                ))
            });
        }
        // If the window reports neither a screen position nor a frame height,
        // `(sx, sy)` is a plausible-looking (0, 0) that would aim the real
        // cursor at raw screen coordinates and click in some other window
        // entirely. Fail instead, so the caller falls back to CDP clicks (and
        // caches `pointer_unmappable`).
        if v.get("positioned").and_then(Value::as_bool) == Some(false) {
            return Err(GolemError::Other(
                "the window reports neither a screen position nor a frame height \
                 (screenX/Y are 0 and outerHeight == innerHeight), and the compositor \
                 could not be asked where the window is -- so viewport coordinates \
                 cannot be mapped to screen pixels"
                    .into(),
            ));
        }
        Ok(PointerMap {
            ox: sx,
            oy: sy,
            css_to_screen: 1.0,
        })
    }

    /// Like [`click_at`](Self::click_at), but moves the REAL OS cursor to the
    /// element and physically clicks it, regardless of the configured input
    /// strategy. Falls back to the CDP `click_at` (with a warning) if native
    /// input is unavailable or fails mid-flight — e.g. the macOS
    /// Accessibility permission was revoked — so a workflow using this never
    /// halts just because the cursor couldn't be driven.
    ///
    /// The browser window must be visible and frontmost at the target point:
    /// a native click lands on whatever is on screen there, including any
    /// window covering it (even Golem's own overlay).
    pub async fn click_at_cursor(&mut self, x: f64, y: f64) -> Result<()> {
        self.guard().await?;
        if !self.input.is_available() {
            self.warn("native input unavailable -- falling back to CDP click");
            return self.click_at_cdp(x, y).await;
        }
        if self.pointer_unmappable {
            return self.click_at_cdp(x, y).await;
        }
        // The real cursor clicks whatever the screen is SHOWING at that point.
        // A browser window parked on a workspace the compositor isn't
        // displaying is therefore not clickable at all: the press lands on
        // whichever window occupies those pixels on the visible workspace --
        // in this project's own setup, Golem's window and the terminals next
        // to it. Unlike an unmappable window this is not a property of the
        // run, so it is re-checked per click and never latched: switching back
        // to the browser's workspace restores native clicking immediately.
        if browser_window_is_showing().await == Some(false) {
            if !self.pointer_offscreen_noted {
                self.pointer_offscreen_noted = true;
                self.warn(
                    "the browser window is on a workspace that isn't being displayed, so a \
                     real-cursor click would land on whatever IS on screen -- using CDP \
                     clicks until it comes back into view",
                );
            }
            return self.click_at_cdp(x, y).await;
        }
        self.pointer_offscreen_noted = false;
        let map = match self.viewport_pointer_map().await {
            Ok(m) => m,
            Err(e) => {
                self.pointer_unmappable = true;
                self.warn(format!(
                    "couldn't map viewport to screen coordinates ({e}) -- using CDP clicks \
                     for the rest of this run"
                ));
                return self.click_at_cdp(x, y).await;
            }
        };
        let target = Point::new(
            map.ox + x * map.css_to_screen,
            map.oy + y * map.css_to_screen,
        );
        self.info(format!(
            "cursor click @ ({x:.0},{y:.0}) -> screen ({:.0},{:.0})",
            target.x, target.y
        ));
        match self.cursor_click_native(target).await {
            Ok(()) => {
                // Keep the CDP-side virtual cursor roughly in sync so a later
                // CDP-humanized move starts from a believable spot.
                self.last_mouse = Point::new(x, y);
                Ok(())
            }
            Err(e) => {
                // Whatever stopped the native cursor (no Wayland protocol for
                // reading the pointer, revoked macOS Accessibility, ...) will
                // stop it for every later click too, so stop retrying it.
                self.pointer_unmappable = true;
                self.warn(format!(
                    "native cursor click failed ({e}) -- using CDP clicks for the rest of \
                     this run"
                ));
                self.click_at_cdp(x, y).await
            }
        }
    }

    /// Humanized native-cursor move + click, all in absolute screen pixels,
    /// starting from wherever the real cursor currently is.
    async fn cursor_click_native(&mut self, target: Point) -> Result<()> {
        // Wayland can't report the cursor position; start from wherever we
        // last drove it (or, on the very first click, skip the approach arc
        // and jump straight to the target).
        let start = match self.input.cursor_pos().await {
            Ok((cx, cy)) => Point::new(cx as f64, cy as f64),
            Err(_) => self.last_native_mouse.unwrap_or(target),
        };
        let cfg = self.cfg();
        let path = {
            let mut rng = rand::rng();
            humanize::mouse_path(start, target, &cfg, &mut rng)
        };
        for s in path {
            self.guard().await?;
            self.input
                .mouse_move_abs(s.point.x.round() as i32, s.point.y.round() as i32)
                .await?;
            if !s.delay.is_zero() {
                tokio::time::sleep(s.delay).await;
            }
        }
        let (settle, hold) = {
            let mut rng = rand::rng();
            (
                humanize::pre_click_settle(&cfg, &mut rng),
                humanize::click_hold(&cfg, &mut rng),
            )
        };
        tokio::time::sleep(settle).await;
        self.guard().await?;
        self.input.mouse_press(MouseButton::Left).await?;
        tokio::time::sleep(hold).await;
        self.input.mouse_release(MouseButton::Left).await?;
        self.last_native_mouse = Some(target);
        Ok(())
    }

    /// Idly drift the REAL OS cursor through a few random nearby waypoints
    /// WITHOUT clicking -- sprinkled between form selections so the pointer
    /// doesn't travel dead bee-lines from one control to the next. Purely
    /// cosmetic: no button is ever pressed, and every real click re-finds
    /// its target's fresh coordinates before pressing, so wandering can
    /// never change which control gets clicked. Best-effort: a quiet no-op
    /// when native input is unavailable or a move fails.
    pub async fn wander_cursor(&mut self) -> Result<()> {
        self.guard().await?;
        if !self.input.is_available() {
            return Ok(());
        }
        let start = match self.input.cursor_pos().await {
            Ok((cx, cy)) => Point::new(cx as f64, cy as f64),
            // Wayland: no way to read the cursor; wander from the last spot
            // we drove it to, or skip quietly if we haven't moved it yet.
            Err(_) => match self.last_native_mouse {
                Some(p) => p,
                None => return Ok(()),
            },
        };
        let cfg = self.cfg();
        let hops = {
            let mut rng = rand::rng();
            rng.random_range(1..=3)
        };
        let mut at = start;
        for _ in 0..hops {
            let (target, pause) = {
                let mut rng = rand::rng();
                let (dx, dy) = humanize::gaussian_jitter(90.0, 260.0, &mut rng);
                (
                    Point::new((at.x + dx).max(4.0), (at.y + dy).max(4.0)),
                    humanize::random_pause(150, 600, &cfg, &mut rng),
                )
            };
            let path = {
                let mut rng = rand::rng();
                humanize::mouse_path(at, target, &cfg, &mut rng)
            };
            for s in path {
                self.guard().await?;
                if self
                    .input
                    .mouse_move_abs(s.point.x.round() as i32, s.point.y.round() as i32)
                    .await
                    .is_err()
                {
                    return Ok(());
                }
                if !s.delay.is_zero() {
                    tokio::time::sleep(s.delay).await;
                }
            }
            at = target;
            tokio::time::sleep(pause).await;
        }
        self.last_native_mouse = Some(at);
        Ok(())
    }

    /// A small idle wheel scroll -- a person glancing further down the page
    /// (or back up at something above) and mostly drifting back. Purely
    /// cosmetic, like [`wander_cursor`](Self::wander_cursor): every real
    /// click re-finds its target's fresh coordinates and each finder scrolls
    /// its element back into view first, so a stray scroll can never change
    /// which control gets clicked. The wheel is dispatched at the virtual
    /// cursor's last position via CDP, never the OS wheel: after a wander
    /// the real cursor may be resting over a different window, and this must
    /// only ever move the task page. Best-effort: a quiet no-op if the wheel
    /// fails.
    pub async fn wander_scroll(&mut self) -> Result<()> {
        self.guard().await?;
        let (notches, up_first, back) = {
            let mut rng = rand::rng();
            (
                rng.random_range(2..=5),
                rng.random_bool(0.3),
                rng.random_bool(0.75),
            )
        };
        let dir: i32 = if up_first { -1 } else { 1 };
        if !self.scroll_notches(dir * notches).await? {
            return Ok(());
        }
        // a beat "reading" whatever the glance landed on
        self.human_pause(400, 1400).await?;
        if back {
            // return most of the way -- humans don't undo a scroll exactly
            let back_notches = {
                let mut rng = rand::rng();
                notches - rng.random_range(0..=1)
            };
            self.scroll_notches(-dir * back_notches).await?;
        }
        Ok(())
    }

    /// `n` wheel notches (positive = down) at the virtual cursor's position,
    /// one notch at a time with a human inter-notch rhythm. Returns whether
    /// every notch dispatched; Stop/Pause still propagate as errors.
    async fn scroll_notches(&mut self, n: i32) -> Result<bool> {
        // ~110 CSS px per notch, matching a typical wheel click
        let delta = if n >= 0 { 110.0 } else { -110.0 };
        for _ in 0..n.abs() {
            self.guard().await?;
            if self
                .browser
                .mouse_wheel(self.last_mouse.x, self.last_mouse.y, 0.0, delta)
                .await
                .is_err()
            {
                return Ok(false);
            }
            let pause = {
                let mut rng = rand::rng();
                humanize::random_pause(40, 140, &self.cfg(), &mut rng)
            };
            tokio::time::sleep(pause).await;
        }
        Ok(true)
    }

    /// Double-click at viewport coordinates.
    pub async fn double_click_at(&mut self, x: f64, y: f64) -> Result<()> {
        let p = Point::new(x, y);
        self.move_to(p).await?;
        self.press_release_at(MouseButton::Left, p).await?;
        let pause = {
            let mut rng = rand::rng();
            humanize::random_pause(40, 120, &self.cfg(), &mut rng)
        };
        tokio::time::sleep(pause).await;
        self.press_release_at(MouseButton::Left, p).await
    }

    /// Type text with human rhythm into whatever is focused.
    pub async fn type_human(&self, text: &str) -> Result<()> {
        let delays = {
            let mut rng = rand::rng();
            humanize::typing_delays(text, &self.cfg(), &mut rng)
        };
        for (c, d) in text.chars().zip(delays) {
            self.guard().await?;
            if self.use_native_keyboard() {
                self.input.key_char(c).await?;
            } else {
                self.browser.key_char(c).await?;
            }
            if !d.is_zero() {
                tokio::time::sleep(d).await;
            }
        }
        Ok(())
    }

    /// Focus a field then type into it.
    pub async fn type_into(&self, selector: &str, text: &str) -> Result<()> {
        self.guard().await?;
        self.browser.focus(selector).await?;
        self.info(format!("type into {selector}"));
        self.type_human(text).await
    }

    /// Press a named key ("Enter", "Tab", "Escape", ...).
    pub async fn press_key(&self, key: &str) -> Result<()> {
        self.guard().await?;
        if self.use_native_keyboard() {
            self.input.key_press(key).await
        } else {
            self.browser.key_press(key).await
        }
    }

    /// Scroll the wheel at viewport point.
    pub async fn scroll(&self, x: f64, y: f64, delta_x: f64, delta_y: f64) -> Result<()> {
        self.guard().await?;
        self.browser.mouse_wheel(x, y, delta_x, delta_y).await
    }

    /// A natural randomized pause.
    pub async fn human_pause(&self, min_ms: u64, max_ms: u64) -> Result<()> {
        self.guard().await?;
        let d = {
            let mut rng = rand::rng();
            humanize::random_pause(min_ms, max_ms, &self.cfg(), &mut rng)
        };
        tokio::time::sleep(d).await;
        self.guard().await
    }

    /// Type a single character as a real keydown (for terminals / browser
    /// editors like neovim). Strategy-aware (native OS keyboard, or CDP keydown).
    /// No internal delay — the caller controls timing.
    pub async fn send_char(&self, c: char) -> Result<()> {
        self.guard().await?;
        if self.use_native_keyboard() {
            self.input.key_char(c).await
        } else {
            self.browser.key_type(c).await
        }
    }

    /// Type a single character as a PHYSICAL key event (scancode + shift), for
    /// remote-desktop streams (Vagon) that forward keystrokes by scancode rather
    /// than the JS `key`/`text`. No internal delay — the caller controls timing.
    pub async fn send_char_vm(&self, c: char) -> Result<()> {
        self.guard().await?;
        self.browser.key_type_physical(c).await
    }

    /// Like [`send_char`](Self::send_char) but holds the key DOWN for `hold`
    /// before releasing — a realistic per-key dwell time. (Native path has no
    /// dwell yet, so it falls back to an instant press.)
    pub async fn send_char_held(&self, c: char, hold: Duration) -> Result<()> {
        self.guard().await?;
        if self.use_native_keyboard() {
            self.input.key_char(c).await
        } else {
            self.browser.key_type_held(c, hold).await
        }
    }

    /// Like [`press_key`](Self::press_key) but holds the key for `hold`.
    pub async fn press_key_held(&self, key: &str, hold: Duration) -> Result<()> {
        self.guard().await?;
        if self.use_native_keyboard() {
            self.input.key_press(key).await
        } else {
            self.browser.key_press_held(key, hold).await
        }
    }

    /// The viewport bounding rect of `selector`, if present.
    pub async fn element_rect(&self, selector: &str) -> Result<Option<crate::geometry::Rect>> {
        self.guard().await?;
        self.browser.get_rect(selector).await
    }

    /// Sleep for `d`, interruptibly: responds to Stop within ~200ms and parks on
    /// Pause. Use for the long human-like pauses in the typing macro.
    pub async fn idle(&self, d: Duration) -> Result<()> {
        let slice = Duration::from_millis(200);
        let mut remaining = d;
        while remaining > Duration::ZERO {
            self.guard().await?;
            let s = remaining.min(slice);
            tokio::time::sleep(s).await;
            remaining = remaining.saturating_sub(s);
        }
        self.guard().await
    }

    // ----- native escape hatches (OS dialogs etc.) ------------------------

    pub async fn native_move(&self, x: i32, y: i32) -> Result<()> {
        self.input.mouse_move_abs(x, y).await
    }
    pub async fn native_click(&self) -> Result<()> {
        self.input.mouse_press(MouseButton::Left).await?;
        let hold = {
            let mut rng = rand::rng();
            humanize::click_hold(&self.cfg(), &mut rng)
        };
        tokio::time::sleep(hold).await;
        self.input.mouse_release(MouseButton::Left).await
    }
    pub async fn native_type(&self, text: &str) -> Result<()> {
        let delays = {
            let mut rng = rand::rng();
            humanize::typing_delays(text, &self.cfg(), &mut rng)
        };
        for (c, d) in text.chars().zip(delays) {
            self.guard().await?;
            self.input.key_char(c).await?;
            if !d.is_zero() {
                tokio::time::sleep(d).await;
            }
        }
        Ok(())
    }
    pub async fn native_key(&self, key: &str) -> Result<()> {
        self.input.key_press(key).await
    }

    // ----- subprocess execution -------------------------------------------

    /// Run a subprocess to completion, streaming its stdout/stderr to the log
    /// and capturing them. Killed if the user presses Stop or `timeout` elapses.
    /// `cwd` sets the working directory; `timeout` of `None` waits indefinitely.
    /// The escape hatch for Docker / Claude Code / ngspice orchestration.
    pub async fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
        timeout: Option<Duration>,
    ) -> Result<CommandOutput> {
        self.guard().await?;
        self.info(format!("$ {program} {}", args.join(" ")));
        let tag = Path::new(program)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(program)
            .to_string();

        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| GolemError::Io(format!("spawn {program}: {e}")))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        // Drain both pipes concurrently, streaming lines and accumulating them.
        let read_fut = async {
            let drain_out = async {
                let mut acc = String::new();
                if let Some(s) = stdout {
                    let mut lines = BufReader::new(s).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        self.emit_proc_line(&tag, &line);
                        acc.push_str(&line);
                        acc.push('\n');
                    }
                }
                acc
            };
            let drain_err = async {
                let mut acc = String::new();
                if let Some(s) = stderr {
                    let mut lines = BufReader::new(s).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        self.emit_proc_line(&tag, &line);
                        acc.push_str(&line);
                        acc.push('\n');
                    }
                }
                acc
            };
            tokio::join!(drain_out, drain_err)
        };

        tokio::select! {
            (out, err) = read_fut => {
                match child.wait().await {
                    Ok(status) => Ok(CommandOutput { code: status.code(), stdout: out, stderr: err }),
                    Err(e) => Err(GolemError::Io(format!("wait {program}: {e}"))),
                }
            }
            _ = self.control.wait_until_stopped() => {
                let _ = child.start_kill();
                Err(GolemError::StoppedByUser)
            }
            _ = sleep_opt(timeout) => {
                let _ = child.start_kill();
                Err(GolemError::Timeout(format!("{program} exceeded its time limit")))
            }
        }
    }

    fn emit_proc_line(&self, tag: &str, line: &str) {
        // Truncate very long lines on a char boundary (never panic on UTF-8).
        let shown: String = line.chars().take(2000).collect();
        self.output(format!("{tag}| {shown}"));
    }

    /// Run a `claude -p --output-format stream-json` agent, parsing the event
    /// stream into readable progress (tool uses + text snippets) and emitting a
    /// periodic elapsed-time heartbeat. Returns the agent's final result text as
    /// `stdout`. Killed on Stop or timeout, like [`run`].
    pub async fn run_claude(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
        timeout: Option<Duration>,
    ) -> Result<CommandOutput> {
        self.guard().await?;
        self.info(format!("$ {program} (agent; streaming progress)"));

        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| GolemError::Io(format!("spawn {program}: {e}")))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let start = tokio::time::Instant::now();

        // `timeout` is an INACTIVITY watchdog, not an absolute cap: it's the max
        // time Claude may produce NO output before we deem it hung. A productive
        // agent streams progress constantly, so a long (multi-hour) solve never
        // trips it, while a genuinely wedged process is still killed. We track the
        // last-output time as ms-since-start in an atomic the drains bump.
        let last_ms = Arc::new(AtomicU64::new(0));

        let read_fut = async {
            let drain_out = async {
                let mut result = String::new();
                if let Some(s) = stdout {
                    let mut lines = BufReader::new(s).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        last_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                        if let Some(p) = claude_progress(&line) {
                            self.output(p);
                        }
                        if let Some(r) = claude_result(&line) {
                            result = r;
                        }
                    }
                }
                result
            };
            let drain_err = async {
                let mut acc = String::new();
                if let Some(s) = stderr {
                    let mut lines = BufReader::new(s).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        last_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                        self.emit_proc_line("claude", &line);
                        acc.push_str(&line);
                        acc.push('\n');
                    }
                }
                acc
            };
            tokio::join!(drain_out, drain_err)
        };
        tokio::pin!(read_fut);

        let mut hb = tokio::time::interval(Duration::from_secs(15));
        hb.tick().await; // discard the immediate first tick

        enum Done {
            Read(String, String),
            Stopped,
            Idle(u64),
        }
        let outcome = loop {
            tokio::select! {
                (out, err) = &mut read_fut => break Done::Read(out, err),
                _ = self.control.wait_until_stopped() => break Done::Stopped,
                _ = hb.tick() => {
                    let now_ms = start.elapsed().as_millis() as u64;
                    let silent_ms = now_ms.saturating_sub(last_ms.load(Ordering::Relaxed));
                    if let Some(idle) = timeout
                        && silent_ms >= idle.as_millis() as u64
                    {
                        break Done::Idle(silent_ms / 1000);
                    }
                    let msg = match timeout {
                        Some(t) => format!(
                            "claude working... {}s elapsed (idle {}s / {}s)",
                            now_ms / 1000,
                            silent_ms / 1000,
                            t.as_secs()
                        ),
                        None => format!("claude working... {}s elapsed", now_ms / 1000),
                    };
                    self.progress(None, msg);
                }
            }
        };

        match outcome {
            Done::Read(out, err) => {
                let code = match child.wait().await {
                    Ok(s) => s.code(),
                    Err(e) => return Err(GolemError::Io(format!("wait {program}: {e}"))),
                };
                Ok(CommandOutput {
                    code,
                    stdout: out,
                    stderr: err,
                })
            }
            Done::Stopped => {
                let _ = child.start_kill();
                Err(GolemError::StoppedByUser)
            }
            Done::Idle(silent_s) => {
                let _ = child.start_kill();
                Err(GolemError::Timeout(format!(
                    "{program} produced no output for {silent_s}s — assumed hung"
                )))
            }
        }
    }

    // ----- clipboard -------------------------------------------------------

    /// Read the OS clipboard (e.g. after the page's "copy" button fired).
    pub fn clipboard_read(&self) -> Result<String> {
        let mut cb = arboard::Clipboard::new()
            .map_err(|e| GolemError::Other(format!("clipboard open: {e}")))?;
        cb.get_text()
            .map_err(|e| GolemError::Other(format!("clipboard read: {e}")))
    }
    pub fn clipboard_write(&self, text: &str) -> Result<()> {
        let mut cb = arboard::Clipboard::new()
            .map_err(|e| GolemError::Other(format!("clipboard open: {e}")))?;
        cb.set_text(text.to_string())
            .map_err(|e| GolemError::Other(format!("clipboard write: {e}")))
    }

    // ----- downloads -------------------------------------------------------

    /// Point the browser's native download behavior (a real file-save
    /// triggered by clicking a link/button, as opposed to `download()`'s HTTP
    /// fetch) at `dir`. Useful when a link needs to be *clicked* rather than
    /// fetched directly -- e.g. a cross-origin or short-lived URL that only
    /// works via a genuine browser-triggered download.
    pub async fn set_download_dir(&self, dir: &std::path::Path) -> Result<()> {
        self.guard().await?;
        std::fs::create_dir_all(dir)
            .map_err(|e| GolemError::Io(format!("mkdir {}: {e}", dir.display())))?;
        self.browser.set_download_dir(dir).await
    }

    /// Download `url` into the configured download dir using the live browser
    /// session's cookies, returning the saved path.
    pub async fn download(&self, url: &str, filename: &str) -> Result<std::path::PathBuf> {
        self.guard().await?;
        let dir = self.settings.download_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| GolemError::Io(format!("mkdir downloads: {e}")))?;
        let cookies = self.browser.cookies_header().await.unwrap_or_default();
        let ua = self.browser.user_agent().await.unwrap_or_default();
        let mut req = self.http.get(url);
        if !cookies.is_empty() {
            req = req.header(reqwest::header::COOKIE, cookies);
        }
        if !ua.is_empty() {
            req = req.header(reqwest::header::USER_AGENT, ua);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| GolemError::Io(format!("download {url}: {e}")))?;
        if !resp.status().is_success() {
            return Err(GolemError::Io(format!(
                "download {url}: HTTP {}",
                resp.status()
            )));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| GolemError::Io(format!("download body {url}: {e}")))?;
        let path = dir.join(sanitize_filename(filename));
        std::fs::write(&path, &bytes)
            .map_err(|e| GolemError::Io(format!("write {}: {e}", path.display())))?;
        self.output(format!(
            "downloaded {} ({} bytes)",
            path.display(),
            bytes.len()
        ));
        Ok(path)
    }
}

/// Char-safe truncation with an ASCII ellipsis.
fn truncate_chars(s: &str, max: usize) -> String {
    let t: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        format!("{t}...")
    } else {
        t
    }
}

/// Turn one `claude --output-format stream-json` line into a readable progress
/// string (tool uses + text snippets), or `None` if it carries no progress.
fn claude_progress(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("type")?.as_str()? != "assistant" {
        return None;
    }
    let content = v.get("message")?.get("content")?.as_array()?;
    let mut out: Vec<String> = Vec::new();
    for block in content {
        match block.get("type").and_then(|x| x.as_str()) {
            Some("tool_use") => {
                let name = block.get("name").and_then(|x| x.as_str()).unwrap_or("tool");
                out.push(format!("  -> {}", tool_summary(name, block.get("input"))));
            }
            Some("text") => {
                let t = block.get("text").and_then(|x| x.as_str()).unwrap_or("").trim();
                if !t.is_empty() {
                    out.push(format!("  claude: {}", truncate_chars(t, 220)));
                }
            }
            _ => {}
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out.join("\n"))
    }
}

/// The final result text from a stream-json `result` event.
fn claude_result(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("type")?.as_str()? == "result" {
        v.get("result").and_then(|x| x.as_str()).map(str::to_string)
    } else {
        None
    }
}

/// A concise one-line summary of a tool use.
fn tool_summary(name: &str, input: Option<&Value>) -> String {
    let field = |k: &str| input.and_then(|i| i.get(k)).and_then(|x| x.as_str()).unwrap_or("");
    match name {
        "Bash" => format!("Bash: {}", truncate_chars(field("command"), 160)),
        "Read" => format!("Read {}", field("file_path")),
        "Write" => format!("Write {}", field("file_path")),
        "Edit" => format!("Edit {}", field("file_path")),
        "Glob" | "Grep" => format!("{name} {}", field("pattern")),
        other => other.to_string(),
    }
}

/// Sleep for `d`, or wait forever if `None` (models "no timeout" in a select).
async fn sleep_opt(d: Option<Duration>) {
    match d {
        Some(d) => tokio::time::sleep(d).await,
        None => std::future::pending::<()>().await,
    }
}

/// Run a `hyprctl ... -j` query and parse its JSON. `None` for anything that
/// isn't a working Hyprland session (the command missing, a non-zero exit,
/// unparseable output), which every caller reads as "the compositor can't be
/// asked".
async fn hyprctl_json(args: &[&str]) -> Option<Value> {
    let out = tokio::process::Command::new("hyprctl")
        .args(args)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// The browser window as the compositor sees it: position and size in global
/// logical screen pixels, plus the workspace and monitor it lives on.
struct CompositorWindow {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    workspace: i64,
    monitor: i64,
}

/// A monitor as the compositor sees it: its scale (logical pixels are
/// physical/scale) and the workspace it is currently displaying.
struct CompositorMonitor {
    scale: f64,
    active_workspace: i64,
}

/// Where the Wayland compositor says the browser window is. Only Hyprland is
/// supported (`hyprctl clients -j`); anywhere else this returns `None` and the
/// caller keeps the browser-reported geometry. When several browser windows
/// exist, the most recently focused one wins (lowest `focusHistoryID`) --
/// native clicks land on the frontmost window anyway.
async fn compositor_browser_window() -> Option<CompositorWindow> {
    let clients = hyprctl_json(&["clients", "-j"]).await?;
    let win = clients
        .as_array()?
        .iter()
        .filter(|c| {
            c.get("mapped").and_then(Value::as_bool).unwrap_or(true)
                && c.get("class")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.to_ascii_lowercase().contains("chrom"))
        })
        .min_by_key(|c| c.get("focusHistoryID").and_then(Value::as_i64).unwrap_or(i64::MAX))?;
    let at = win.get("at")?.as_array()?;
    let size = win.get("size")?.as_array()?;
    Some(CompositorWindow {
        x: at.first()?.as_f64()?,
        y: at.get(1)?.as_f64()?,
        w: size.first()?.as_f64()?,
        h: size.get(1)?.as_f64()?,
        workspace: win.get("workspace")?.get("id")?.as_i64()?,
        monitor: win.get("monitor").and_then(Value::as_i64).unwrap_or(-1),
    })
}

/// The compositor's view of monitor `id` (or the focused one, when the window
/// didn't say which monitor it is on).
async fn compositor_monitor(id: i64) -> Option<CompositorMonitor> {
    let monitors = hyprctl_json(&["monitors", "-j"]).await?;
    let monitors = monitors.as_array()?;
    let mon = monitors
        .iter()
        .find(|m| m.get("id").and_then(Value::as_i64) == Some(id))
        .or_else(|| {
            monitors
                .iter()
                .find(|m| m.get("focused").and_then(Value::as_bool) == Some(true))
        })
        .or_else(|| monitors.first())?;
    Some(CompositorMonitor {
        scale: mon
            .get("scale")
            .and_then(Value::as_f64)
            .filter(|s| *s > 0.0)
            .unwrap_or(1.0),
        active_workspace: mon.get("activeWorkspace")?.get("id")?.as_i64()?,
    })
}

/// Whether the browser window is on a workspace the compositor is actually
/// displaying — i.e. whether a real-cursor click aimed at it would reach it.
/// `None` when there is no compositor to ask (X11, macOS, a non-Hyprland
/// Wayland session), which callers treat as "assume it is showing" so nothing
/// changes on those platforms.
async fn browser_window_is_showing() -> Option<bool> {
    if !is_wayland_session() {
        return None;
    }
    let win = compositor_browser_window().await?;
    let mon = compositor_monitor(win.monitor).await?;
    Some(win.workspace == mon.active_workspace)
}

/// Where the page viewport sits on screen, and how far one CSS pixel reaches
/// there: `screen = origin + css * css_to_screen`. Built by
/// [`WorkflowCtx::viewport_pointer_map`], which documents where each part
/// comes from.
struct PointerMap {
    ox: f64,
    oy: f64,
    /// Screen pixels per CSS pixel — the page zoom, 1.0 at 100%.
    css_to_screen: f64,
}

/// Locate the viewport inside the compositor's window rectangle, given the
/// page's own `innerWidth`/`innerHeight` (CSS pixels) and the page zoom.
///
/// `iw * zoom` by `ih * zoom` is the viewport in the compositor's pixels;
/// whatever is left of the window around it is the browser's frame. Chromium
/// puts that frame above the viewport, so any leftover WIDTH is a side border
/// (split evenly, and the same border runs along the bottom) and everything
/// else is the tab strip and URL bar on top.
///
/// `None` when the numbers can't describe one window — a viewport wider or
/// taller than the window it is supposedly inside, or a frame taller than the
/// window is. That means some assumption here is wrong (docked devtools, a
/// second browser window picked by mistake, a compositor reporting physical
/// pixels), and a wrong-but-plausible answer would aim the real cursor at
/// whatever happens to sit at those coordinates.
fn viewport_map_in_window(
    win: &CompositorWindow,
    iw: f64,
    ih: f64,
    zoom: f64,
) -> Option<PointerMap> {
    let (vw, vh) = (iw * zoom, ih * zoom);
    let side = ((win.w - vw) / 2.0).max(0.0);
    let top = win.h - vh - side;
    let plausible = (0.1..10.0).contains(&zoom)
        && vw <= win.w + 2.0
        && vh <= win.h + 2.0
        && (-2.0..=win.h * 0.6).contains(&top);
    plausible.then(|| PointerMap {
        ox: win.x + side,
        oy: win.y + top,
        css_to_screen: zoom,
    })
}

/// Strip path separators and other awkward characters from a download filename.
fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    if trimmed.is_empty() {
        "download.bin".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The window Golem drives on the machine this was measured on: Hyprland
    /// at scale 1.5, a maximized Chromium tiled under a 35px bar with 22px
    /// gaps, all in logical pixels (`hyprctl clients -j`).
    fn window() -> CompositorWindow {
        CompositorWindow {
            x: 22.0,
            y: 57.0,
            w: 1236.0,
            h: 641.0,
            workspace: 3,
            monitor: 0,
        }
    }

    /// At 100% zoom the viewport is the full window width, and the ~87px the
    /// window has left over is the tab strip and URL bar above it.
    #[test]
    fn maps_an_unzoomed_page_below_the_browser_frame() {
        let m = viewport_map_in_window(&window(), 1236.0, 554.0, 1.0).expect("maps");
        assert!((m.ox - 22.0).abs() < 1.0, "ox = {}", m.ox);
        assert!((m.oy - 144.0).abs() < 1.0, "oy = {}", m.oy);
        assert!((m.css_to_screen - 1.0).abs() < f64::EPSILON);
    }

    /// The same window with the page zoomed to 110% (devicePixelRatio 1.65 on
    /// a 1.5x monitor): innerWidth/Height shrink to CSS pixels, so the
    /// viewport must be scaled back up before it can be located -- and it
    /// lands in exactly the same place, because it is the same window. This is
    /// what the old `outerHeight - innerHeight` arithmetic got wrong: it read
    /// the zoom as a 138px frame and aimed every click ~54px high.
    #[test]
    fn a_zoomed_page_maps_to_the_same_viewport_origin() {
        let plain = viewport_map_in_window(&window(), 1236.0, 554.0, 1.0).expect("maps");
        let zoomed = viewport_map_in_window(&window(), 1123.0, 503.0, 1.65 / 1.5).expect("maps");
        assert!((zoomed.ox - plain.ox).abs() < 1.5, "ox = {}", zoomed.ox);
        assert!((zoomed.oy - plain.oy).abs() < 1.5, "oy = {}", zoomed.oy);
        assert!((zoomed.css_to_screen - 1.1).abs() < 0.001);
        // A button at CSS (561, 281) -- where "Confirm time" sits -- is inside
        // the window, not 34px above it in the page's own chrome.
        let (x, y) = (
            zoomed.ox + 561.0 * zoomed.css_to_screen,
            zoomed.oy + 281.0 * zoomed.css_to_screen,
        );
        let w = window();
        assert!(x > w.x && x < w.x + w.w, "x = {x}");
        assert!(y > w.y && y < w.y + w.h, "y = {y}");
    }

    /// A viewport that doesn't fit its window means one of the inputs isn't
    /// describing what we think it is, so there is no map to return.
    #[test]
    fn refuses_a_viewport_that_cannot_fit_its_window() {
        assert!(viewport_map_in_window(&window(), 1920.0, 554.0, 1.0).is_none());
        assert!(viewport_map_in_window(&window(), 1236.0, 700.0, 1.0).is_none());
        // A frame taller than half the window: the window rectangle and the
        // page are not the same window.
        assert!(viewport_map_in_window(&window(), 1236.0, 100.0, 1.0).is_none());
    }
}
