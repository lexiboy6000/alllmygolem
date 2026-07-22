//! The message contract between the GUI thread and the engine (which runs on a
//! background tokio runtime). They never touch each other's state directly:
//! the GUI sends [`UiCommand`]s and drains [`EngineEvent`]s once per frame.
//!
//! Both directions use tokio unbounded MPSC channels. `UnboundedSender::send`
//! is synchronous, so the GUI (non-async) can send freely; the GUI drains
//! events with `try_recv` each `update()`.

use std::collections::BTreeMap;

use uuid::Uuid;

use crate::settings::Settings;

pub type EventTx = tokio::sync::mpsc::UnboundedSender<EngineEvent>;
pub type EventRx = tokio::sync::mpsc::UnboundedReceiver<EngineEvent>;
pub type CommandTx = tokio::sync::mpsc::UnboundedSender<UiCommand>;
pub type CommandRx = tokio::sync::mpsc::UnboundedReceiver<UiCommand>;

/// High-level engine lifecycle state, shown in the main window and overlay.
#[derive(Clone, Debug, PartialEq)]
pub enum EngineStatus {
    Idle,
    Connecting,
    Connected,
    Running { workflow: String, step: String },
    Paused,
    Reconnecting { attempt: u32 },
    Stopped,
    Errored(String),
}

/// Connection state to Chrome, surfaced separately so the UI can show an
/// attach/reconnect indicator independent of workflow state.
#[derive(Clone, Debug, PartialEq)]
pub enum ConnState {
    Disconnected,
    Connecting,
    Connected { target_url: Option<String> },
    Reconnecting { attempt: u32 },
    Relaunching,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// What kind of answer a prompt expects.
#[derive(Clone, Debug, PartialEq)]
pub enum PromptKind {
    /// Free text, with an optional pre-filled default.
    Text { default: String },
    /// Yes/no.
    Confirm,
    /// Acknowledge-only (the spec's "warn user"); resolves on dismiss.
    Info,
    /// Pick one of several options.
    Choice { options: Vec<String> },
}

/// A request for user input/decision. The workflow blocks until a matching
/// [`UiCommand::PromptResponse`] (or a Stop) arrives.
#[derive(Clone, Debug)]
pub struct PromptRequest {
    pub id: Uuid,
    pub message: String,
    pub kind: PromptKind,
}

/// The user's answer to a prompt.
#[derive(Clone, Debug)]
pub enum PromptResponse {
    Text(String),
    Bool(bool),
    Choice(usize),
    /// Dismissed (Info acknowledged, or cancelled).
    Dismiss,
}

/// Summary of how a workflow finished, for display/logging.
#[derive(Clone, Debug)]
pub enum OutcomeSummary {
    Completed,
    /// Completed with a short data summary (e.g. extracted task id).
    CompletedWith(String),
    Halted(String),
    Stopped,
    Failed(String),
}

/// Engine → GUI.
#[derive(Clone, Debug)]
pub enum EngineEvent {
    Status(EngineStatus),
    Connection(ConnState),
    /// Progress for the current workflow. `fraction` is `None` for
    /// indeterminate work.
    Progress { fraction: Option<f32>, label: String },
    /// User-facing output line.
    Output(String),
    /// Diagnostic log line (also written to the on-disk log).
    Log { level: LogLevel, message: String },
    /// A non-fatal error worth surfacing.
    Error(String),
    /// Engine needs the user to respond.
    Prompt(PromptRequest),
    /// A previously shown prompt no longer needs an answer (e.g. run stopped).
    PromptCancelled(Uuid),
    WorkflowStarted { name: String },
    WorkflowFinished { name: String, outcome: OutcomeSummary },
    /// The full requested chain finished (idle again).
    ChainFinished,
    /// A panic was caught and isolated; the process kept running.
    PanicCaught(String),
    /// Snapshot of available workflows (sent once at startup / on change).
    Workflows(Vec<crate::registry::WorkflowInfo>),
    /// A resumable checkpoint exists the user may continue.
    ResumeAvailable { run_id: String, workflow: String, step: String },
    /// A workflow asks the GUI to persist a new default value for one of its
    /// inputs (e.g. "make this git SHA the default").
    SetWorkflowInput {
        workflow: String,
        key: String,
        value: String,
    },
}

/// GUI → Engine.
#[derive(Clone, Debug)]
pub enum UiCommand {
    /// (Re)connect to Chrome.
    Connect,
    /// Launch Chrome/Chromium with the debug port, then connect. `user_data_dir`
    /// selects which profile to launch (None = the configured/default one).
    /// Chrome first, Chromium as a fallback.
    LaunchChrome { user_data_dir: Option<String> },
    Disconnect,
    /// Run a single workflow (its dependencies run first) with optional inputs.
    Run {
        workflow: String,
        inputs: BTreeMap<String, String>,
    },
    /// Run an explicit ordered list of workflows back-to-back, sharing `inputs`
    /// across all of them. Unlike [`Run`], resolved prerequisites run WITHOUT a
    /// confirm prompt — the caller (e.g. the pipeline) has already chosen the set.
    RunChain {
        workflows: Vec<String>,
        inputs: BTreeMap<String, String>,
    },
    Stop,
    Pause,
    Resume,
    /// Answer to a pending prompt.
    PromptResponse { id: Uuid, value: PromptResponse },
    /// Apply edited settings.
    UpdateSettings(Box<Settings>),
    /// Resume a workflow from a saved checkpoint.
    ResumeCheckpoint { run_id: String },
    /// Graceful shutdown (window closing).
    Shutdown,
}
