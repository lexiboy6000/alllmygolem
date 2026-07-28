//! Persistent user settings. Loaded at startup, editable from the GUI settings
//! panel, and snapshotted into each workflow run.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{GolemError, Result};
use crate::humanize::HumanizeConfig;

/// How clicks/typing reach the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[derive(Default)]
pub enum InputStrategy {
    /// Dispatch human-like movement + clicks via CDP `Input.*`. Default: robust
    /// for unattended runs, no window focus needed, works into iframes by
    /// coordinate.
    #[default]
    Cdp,
    /// Drive the real OS cursor / keyboard via the native backend.
    Native,
    /// Native cursor movement + clicks, CDP for DOM/navigation/clipboard.
    Hybrid,
}


#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    // --- Chrome connection ---
    /// Host serving the DevTools endpoint.
    pub chrome_host: String,
    /// `--remote-debugging-port` of the running Chrome.
    pub chrome_port: u16,
    /// Path to the Chrome/Chromium binary, used only if `auto_relaunch_chrome`
    /// is enabled and the connection is lost. `None` = auto-detect.
    pub chrome_path: Option<String>,
    /// `--user-data-dir` to pass when relaunching (keeps the logged-in profile).
    pub chrome_user_data_dir: Option<String>,
    /// If Chrome dies mid-run, relaunch it with the debug flag and re-attach.
    pub auto_relaunch_chrome: bool,

    // --- Filesystem ---
    /// Root for all Golem output (logs, checkpoints, downloads).
    pub output_dir: PathBuf,

    // --- Behaviour ---
    /// Offer to resume from the latest checkpoint on startup / after a crash.
    pub auto_resume: bool,
    /// Global UI zoom factor for the GUI (1.0 = egui default). Bigger = larger
    /// text and widgets.
    pub zoom: f32,
    /// How input is delivered.
    pub input_strategy: InputStrategy,
    /// Human-motion tuning.
    pub humanize: HumanizeConfig,

    // --- Timeouts / reconnect (milliseconds) ---
    pub cdp_call_timeout_ms: u64,
    pub default_wait_timeout_ms: u64,
    pub reconnect_initial_ms: u64,
    pub reconnect_max_ms: u64,
    pub reconnect_max_attempts: u32,

    // --- Solve workflow (Docker + Claude Code) ---
    /// Path/command for the Claude Code CLI.
    pub claude_path: String,
    /// Docker image tag for the ngspice sandbox.
    pub docker_image: String,
    /// Max solve→review iterations before stopping and prompting.
    pub solve_max_iterations: u32,
    /// `claude` INACTIVITY timeout (seconds): max time the agent may produce no
    /// output before it's considered hung and killed. NOT an absolute cap — a
    /// productive multi-hour solve streams output the whole time and never trips
    /// it; only a genuinely wedged process does.
    pub claude_timeout_secs: u64,
    /// Model for the solve/review agents (`--model`); alias like "opus" or a full
    /// id like "claude-opus-4-7". Empty = Claude Code's default.
    pub solve_model: String,
    /// Reasoning effort for the solve/review agents (`--effort`): low/medium/high/
    /// xhigh/max. Empty = Claude Code's default.
    pub solve_effort: String,

    // --- Task pipeline ---
    /// Automatic mode for the task pipeline (workflows 1-8 of `first_test`) and
    /// every subworkflow it pulls in: none of its human gates block. The "GOLEM
    /// NEEDS YOU" review self-approves, the submit confirm self-answers, and the
    /// "fix it on the page, then dismiss" repair prompts become warning lines.
    /// Deliberately scoped to that pipeline -- the feather/complete/solve
    /// families keep their own prompts regardless of this flag.
    ///
    /// With this on, evaluations are submitted to the live platform with no
    /// human ever seeing them, so it defaults to off.
    pub auto_mode: bool,

    // --- GUI ---
    /// Show the movable always-on-top overlay while a workflow runs. Disable if
    /// it interferes with focus/selection on your compositor (some Wayland setups).
    pub overlay_enabled: bool,
    /// Persisted per-workflow input values (workflow name -> key -> value), so
    /// the last-entered inputs (expected SHA, task id, …) survive restarts.
    pub workflow_inputs: BTreeMap<String, BTreeMap<String, String>>,
}

impl Default for Settings {
    fn default() -> Self {
        let output_dir = PathBuf::from("golem-output");
        Settings {
            chrome_host: "localhost".to_string(),
            chrome_port: 9222,
            chrome_path: None,
            chrome_user_data_dir: None,
            auto_relaunch_chrome: true,
            output_dir,
            auto_resume: true,
            zoom: 1.3,
            input_strategy: InputStrategy::default(),
            humanize: HumanizeConfig::default(),
            cdp_call_timeout_ms: 15_000,
            default_wait_timeout_ms: 20_000,
            reconnect_initial_ms: 500,
            reconnect_max_ms: 30_000,
            reconnect_max_attempts: 0, // 0 = retry forever (multi-hour runs)
            claude_path: "claude".to_string(),
            docker_image: "golem-ngspice:latest".to_string(),
            solve_max_iterations: 5,
            claude_timeout_secs: 900,
            solve_model: "opus".to_string(),
            solve_effort: "high".to_string(),
            // Off by default: on, nobody reviews an evaluation before it is
            // submitted for real.
            auto_mode: false,
            // Opt-in: the overlay is a separate always-on-top window, which on
            // Wayland can steal focus and get flagged "not responding" by the
            // compositor while Golem is on another workspace (the main app is
            // unaffected). Enable it in Settings if you want the floating status.
            overlay_enabled: false,
            workflow_inputs: BTreeMap::new(),
        }
    }
}

impl Settings {
    pub fn checkpoint_dir(&self) -> PathBuf {
        self.output_dir.join("checkpoints")
    }
    pub fn download_dir(&self) -> PathBuf {
        self.output_dir.join("downloads")
    }
    pub fn log_dir(&self) -> PathBuf {
        self.output_dir.join("logs")
    }
    pub fn data_dir(&self) -> PathBuf {
        self.output_dir.join("data")
    }

    /// The DevTools HTTP base, e.g. `http://localhost:9222`.
    pub fn devtools_http(&self) -> String {
        format!("http://{}:{}", self.chrome_host, self.chrome_port)
    }

    /// Where the settings file lives. Falls back to the working directory if a
    /// platform config dir can't be determined.
    pub fn config_path() -> PathBuf {
        if let Some(dirs) = directories::ProjectDirs::from("dev", "golem", "Golem") {
            return dirs.config_dir().join("settings.json");
        }
        PathBuf::from("golem-settings.json")
    }

    /// Load settings, returning defaults if the file is missing or unreadable.
    /// Never panics; a corrupt file degrades to defaults with the error logged
    /// by the caller.
    pub fn load() -> Self {
        let path = Self::config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!("settings parse failed ({e}); using defaults");
                Settings::default()
            }),
            Err(_) => Settings::default(),
        }
    }

    /// Persist settings to disk.
    pub fn save(&self) -> Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, text)?;
        Ok(())
    }

    /// Create the output directory tree. Called once at startup.
    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            self.output_dir.clone(),
            self.checkpoint_dir(),
            self.download_dir(),
            self.log_dir(),
            self.data_dir(),
        ] {
            std::fs::create_dir_all(&dir)
                .map_err(|e| GolemError::Io(format!("create {}: {e}", dir.display())))?;
        }
        Ok(())
    }
}
