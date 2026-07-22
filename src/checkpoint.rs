//! Crash-resilient checkpoints. Per `docs/STABILITY.md` we persist after every
//! step so a restart resumes mid-workflow instead of losing hours. Writes are
//! atomic (write-temp-then-rename) so a crash *during* a write can't corrupt an
//! existing checkpoint.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{GolemError, Result};

/// The resumable state of one workflow run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunState {
    pub run_id: String,
    pub workflow: String,
    /// Number of steps completed so far.
    pub step_index: usize,
    /// Name of the most recently completed step.
    pub step_name: String,
    /// Workflow-defined variables (extracted data, flags).
    pub store: BTreeMap<String, Value>,
    /// Inputs the run was started with.
    pub inputs: BTreeMap<String, String>,
    /// RFC3339 timestamp of the last update.
    pub updated: String,
    /// Free-form status string ("running", "completed", "halted", ...).
    pub status: String,
}

impl RunState {
    pub fn file_path(dir: &Path, run_id: &str) -> PathBuf {
        dir.join(format!("{run_id}.json"))
    }

    /// Atomically write this checkpoint into `dir`.
    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)
            .map_err(|e| GolemError::Checkpoint(format!("mkdir {}: {e}", dir.display())))?;
        let final_path = Self::file_path(dir, &self.run_id);
        let tmp_path = dir.join(format!(".{}.tmp", self.run_id));
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| GolemError::Checkpoint(format!("serialize: {e}")))?;
        std::fs::write(&tmp_path, text.as_bytes())
            .map_err(|e| GolemError::Checkpoint(format!("write tmp: {e}")))?;
        std::fs::rename(&tmp_path, &final_path)
            .map_err(|e| GolemError::Checkpoint(format!("rename: {e}")))?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<RunState> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| GolemError::Checkpoint(format!("read {}: {e}", path.display())))?;
        serde_json::from_str(&text)
            .map_err(|e| GolemError::Checkpoint(format!("parse {}: {e}", path.display())))
    }

    /// All checkpoints in `dir`, newest first by `updated`.
    pub fn list(dir: &Path) -> Result<Vec<RunState>> {
        let mut out = Vec::new();
        let read = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(_) => return Ok(out), // no dir yet = no checkpoints
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json")
                && let Ok(rs) = RunState::load(&path) {
                    out.push(rs);
                }
        }
        out.sort_by(|a, b| b.updated.cmp(&a.updated));
        Ok(out)
    }

    /// The most recently updated checkpoint, if any.
    pub fn latest(dir: &Path) -> Result<Option<RunState>> {
        Ok(Self::list(dir)?.into_iter().next())
    }

    /// Remove a checkpoint (e.g. once its run completed).
    pub fn delete(dir: &Path, run_id: &str) -> Result<()> {
        let path = Self::file_path(dir, run_id);
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|e| GolemError::Checkpoint(format!("remove: {e}")))?;
        }
        Ok(())
    }
}
