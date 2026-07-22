//! The `Workflow` trait. A new workflow is one small Rust file: implement this
//! trait and register it in `crate::workflows`. The `run` method gets a
//! `&mut WorkflowCtx` exposing the full human-input + browser API.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::context::WorkflowCtx;
use crate::error::Result;

/// What a workflow produced.
#[derive(Clone, Debug)]
pub enum WorkflowOutcome {
    /// Done, nothing to return.
    Completed,
    /// Done, with structured return data (e.g. extracted task data).
    CompletedWith(Value),
}

/// Declares an input a workflow accepts (shown as a field in the GUI before
/// running, e.g. the task URL for "Navigate to task").
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InputSpec {
    pub key: String,
    pub label: String,
    pub required: bool,
    pub default: Option<String>,
}

impl InputSpec {
    pub fn required(key: &str, label: &str) -> Self {
        InputSpec {
            key: key.to_string(),
            label: label.to_string(),
            required: true,
            default: None,
        }
    }
    pub fn optional(key: &str, label: &str, default: &str) -> Self {
        InputSpec {
            key: key.to_string(),
            label: label.to_string(),
            required: false,
            default: Some(default.to_string()),
        }
    }
}

#[async_trait]
pub trait Workflow: Send + Sync {
    /// Unique, stable, human-readable name (matches the spec headings).
    fn name(&self) -> &'static str;

    /// One-line description for the GUI.
    fn description(&self) -> &'static str {
        ""
    }

    /// Workflows that must run (and succeed) before this one. The engine runs
    /// them first in dependency order.
    fn dependencies(&self) -> Vec<&'static str> {
        Vec::new()
    }

    /// Workflows to automatically queue after this one completes.
    fn run_after(&self) -> Vec<&'static str> {
        Vec::new()
    }

    /// Inputs this workflow accepts.
    fn inputs(&self) -> Vec<InputSpec> {
        Vec::new()
    }

    /// Whether this workflow needs a connected Chrome session. Browser-driven
    /// workflows return `true` (the default); local-only workflows (e.g. the
    /// Docker/Claude solve pipeline) return `false` so the engine runs them
    /// without requiring Connect.
    fn requires_browser(&self) -> bool {
        true
    }

    /// Execute. Return `Err(GolemError::Halted(..))` (e.g. via
    /// `ctx.stop_and_warn`) for the spec's "STOP and warn user"; propagate
    /// `GolemError::StoppedByUser` untouched.
    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome>;
}
