//! Everything a workflow file needs. A new workflow starts with:
//! `use crate::prelude::*;`
//!
//! This is a curated authoring surface; an individual workflow may not use every
//! re-export, so unused-import noise is suppressed here.
#![allow(unused_imports)]

pub use std::sync::Arc;
pub use std::time::Duration;

pub use async_trait::async_trait;
pub use serde_json::{Value, json};

pub use crate::backend::{BrowserBackend, InputBackend, MouseButton};
pub use crate::context::{CommandOutput, WorkflowCtx};
pub use crate::error::{GolemError, Result};
pub use crate::geometry::{Point, Rect};
pub use crate::workflow::{InputSpec, Workflow, WorkflowOutcome};
