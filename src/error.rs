//! Error type for the whole application.
//!
//! Stability is the #1 requirement (see `docs/STABILITY.md`). Every fallible
//! operation returns [`Result`] so nothing in the engine hot path needs to
//! `unwrap`/`expect`/`panic!`. The variants are deliberately coarse — callers
//! mostly need to know *whether* to stop, retry, or surface a message.

use thiserror::Error;

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, GolemError>;

/// All errors flow through this type. It is `Clone` so it can be carried in
/// [`crate::messages::EngineEvent`] across the engine→GUI channel.
#[derive(Debug, Clone, Error)]
pub enum GolemError {
    /// The user pressed Stop. Workflows should propagate this untouched so the
    /// engine can halt the whole chain cleanly.
    #[error("stopped by user")]
    StoppedByUser,

    /// A workflow deliberately halted (the spec's "STOP and warn/prompt user").
    /// Carries the human-readable reason to show.
    #[error("halted: {0}")]
    Halted(String),

    /// An operation exceeded its deadline.
    #[error("timed out: {0}")]
    Timeout(String),

    /// A selector matched nothing.
    #[error("element not found: {0}")]
    NotFound(String),

    /// Anything from the CDP/browser layer.
    #[error("browser error: {0}")]
    Browser(String),

    /// Anything from the native input layer.
    #[error("input error: {0}")]
    Input(String),

    /// Connection lost / could not attach / reconnect failed.
    #[error("connection error: {0}")]
    Connection(String),

    /// Filesystem / IO.
    #[error("io error: {0}")]
    Io(String),

    /// Checkpoint persistence problem.
    #[error("checkpoint error: {0}")]
    Checkpoint(String),

    /// A user prompt could not be delivered or was abandoned.
    #[error("prompt error: {0}")]
    Prompt(String),

    /// A stub / not-yet-implemented path. Returned (never panicked) so the
    /// process keeps running.
    #[error("not implemented: {0}")]
    NotImplemented(String),

    /// Catch-all with a message.
    #[error("{0}")]
    Other(String),
}

impl GolemError {
    /// Convenience constructor for an [`GolemError::Other`].
    pub fn other(msg: impl std::fmt::Display) -> Self {
        GolemError::Other(msg.to_string())
    }

    /// `true` if this error means the run should stop entirely rather than be
    /// retried (user stop or a deliberate halt).
    pub fn is_terminal(&self) -> bool {
        matches!(self, GolemError::StoppedByUser | GolemError::Halted(_))
    }
}

impl From<std::io::Error> for GolemError {
    fn from(e: std::io::Error) -> Self {
        GolemError::Io(e.to_string())
    }
}

impl From<serde_json::Error> for GolemError {
    fn from(e: serde_json::Error) -> Self {
        GolemError::Other(format!("json: {e}"))
    }
}

impl From<anyhow::Error> for GolemError {
    fn from(e: anyhow::Error) -> Self {
        GolemError::Other(e.to_string())
    }
}
