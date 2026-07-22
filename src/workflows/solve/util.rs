//! Shared helpers for the solve workflows: task-bundle loading, Docker/Claude
//! plumbing, workspace staging, the agent prompts, and verdict parsing.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::prelude::*;
use crate::settings::Settings;

/// Dockerfile for the ngspice sandbox image. Built by preflight if missing.
pub const DOCKERFILE: &str = "FROM ubuntu:24.04\n\
RUN apt-get update && apt-get install -y --no-install-recommends \\\n\
      ngspice gnuplot ca-certificates xvfb \\\n\
    && rm -rf /var/lib/apt/lists/*\n\
WORKDIR /work\n";

/// The downloaded task bundle written by "Get task data".
#[derive(Deserialize, Clone, Default)]
pub struct TaskBundle {
    #[serde(default)]
    pub task_url: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub has_starting_state: bool,
    #[serde(default)]
    pub starting_state: String,
    #[serde(default)]
    pub rubric: serde_json::Value,
    #[serde(default)]
    pub reference_files: Vec<FileEntry>,
    #[serde(default)]
    pub starting_state_files: Vec<FileEntry>,
    /// Operator-estimated hours from the task's "anticipate … (in hours)" field,
    /// when present. Sizes the checkpoint count (`ceil(hours)+2`).
    #[serde(default)]
    pub anticipated_hours: Option<f64>,
}

#[derive(Deserialize, Clone, Default)]
pub struct FileEntry {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub url: String,
}

/// Locate and parse a task bundle: by explicit `task_id`, else the newest
/// `task-*.json` in the data dir. Returns `(task_id, bundle)`.
pub fn find_bundle(settings: &Settings, task_id: Option<&str>) -> Result<(String, TaskBundle)> {
    let dir = settings.data_dir();
    let path = match task_id {
        Some(id) if !id.trim().is_empty() => dir.join(format!("task-{}.json", id.trim())),
        _ => newest_bundle(&dir)?,
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|e| GolemError::Io(format!("read bundle {}: {e}", path.display())))?;
    let bundle: TaskBundle =
        serde_json::from_str(&text).map_err(|e| GolemError::Other(format!("parse bundle: {e}")))?;
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.trim_start_matches("task-").to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "task".to_string());
    Ok((id, bundle))
}

fn newest_bundle(dir: &Path) -> Result<PathBuf> {
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    let read = std::fs::read_dir(dir)
        .map_err(|e| GolemError::Io(format!("read data dir {}: {e}", dir.display())))?;
    for entry in read.flatten() {
        let p = entry.path();
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name.starts_with("task-") && name.ends_with(".json")
            && let Ok(modified) = entry.metadata().and_then(|m| m.modified())
                && newest.as_ref().map(|(t, _)| modified > *t).unwrap_or(true) {
                    newest = Some((modified, p));
                }
    }
    newest.map(|(_, p)| p).ok_or_else(|| {
        GolemError::Other("no task bundle found in the data dir; run 'Get task data' first".into())
    })
}

/// Whether the ngspice image exists locally.
pub async fn image_exists(ctx: &WorkflowCtx) -> bool {
    let image = image_tag(&ctx.settings);
    matches!(
        ctx.run(
            "docker",
            &["image", "inspect", image.as_str()],
            None,
            Some(Duration::from_secs(25)),
        )
        .await,
        Ok(o) if o.success()
    )
}

/// Build (or rebuild) the ngspice image from the embedded Dockerfile.
pub async fn build_image(ctx: &WorkflowCtx) -> Result<()> {
    let image = image_tag(&ctx.settings);
    let build_dir = ctx.settings.output_dir.join("docker");
    std::fs::create_dir_all(&build_dir)
        .map_err(|e| GolemError::Io(format!("mkdir docker build dir: {e}")))?;
    std::fs::write(build_dir.join("Dockerfile"), DOCKERFILE)
        .map_err(|e| GolemError::Io(format!("write Dockerfile: {e}")))?;
    let build_dir_str = build_dir.to_string_lossy().to_string();
    let out = ctx
        .run(
            "docker",
            &["build", "-t", image.as_str(), build_dir_str.as_str()],
            None,
            Some(Duration::from_secs(1200)),
        )
        .await?;
    if out.success() {
        Ok(())
    } else {
        Err(GolemError::Other(format!(
            "docker build failed: {}",
            out.combined().trim()
        )))
    }
}

pub fn image_tag(settings: &Settings) -> String {
    if settings.docker_image.trim().is_empty() {
        "golem-ngspice:latest".to_string()
    } else {
        settings.docker_image.clone()
    }
}

pub fn claude_bin(settings: &Settings) -> String {
    if settings.claude_path.trim().is_empty() {
        "claude".to_string()
    } else {
        settings.claude_path.clone()
    }
}

pub fn container_name(task_id: &str) -> String {
    let cleaned: String = task_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    format!("golem-ngspice-{cleaned}")
}

/// `uid:gid` of the current host user, for `docker run --user` so container-
/// written files are owned by the user (not root). `None` off Unix.
#[cfg(unix)]
pub fn host_user() -> Option<String> {
    // getuid/getgid never fail and have no preconditions.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    Some(format!("{uid}:{gid}"))
}
#[cfg(not(unix))]
pub fn host_user() -> Option<String> {
    None
}

/// Absolute path for a (possibly relative) workspace, without the Windows
/// `\\?\` verbatim prefix that `canonicalize` adds (Docker `-v` dislikes it).
pub fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        let cwd = std::env::current_dir()
            .map_err(|e| GolemError::Io(format!("current dir: {e}")))?;
        Ok(cwd.join(path))
    }
}

/// Stage the workspace: write prompt/rubric/starting-state/INSTRUCTIONS and copy
/// referenced files in from the downloads dir.
pub fn stage_workspace(
    ctx: &WorkflowCtx,
    ws: &Path,
    bundle: &TaskBundle,
    container: &str,
) -> Result<()> {
    std::fs::create_dir_all(ws).map_err(|e| GolemError::Io(format!("mkdir workspace: {e}")))?;

    write_file(ws, "prompt.txt", &bundle.prompt)?;
    let rubric = serde_json::to_string_pretty(&bundle.rubric).unwrap_or_else(|_| "{}".to_string());
    write_file(ws, "rubric.json", &rubric)?;
    write_file(ws, "starting_state.txt", &bundle.starting_state)?;
    write_file(ws, "INSTRUCTIONS.md", &instructions_md(container))?;

    let downloads = ctx.settings.download_dir();
    let mut copied = 0usize;
    for fe in bundle.reference_files.iter().chain(bundle.starting_state_files.iter()) {
        if fe.name.trim().is_empty() {
            continue;
        }
        let src = downloads.join(&fe.name);
        if src.exists() {
            let dst = ws.join(&fe.name);
            match std::fs::copy(&src, &dst) {
                Ok(_) => copied += 1,
                Err(e) => ctx.warn(format!("could not copy {}: {e}", fe.name)),
            }
        } else {
            ctx.warn(format!("referenced file missing from downloads: {}", fe.name));
        }
    }
    ctx.output(format!("staged workspace ({copied} file(s)) at {}", ws.display()));
    Ok(())
}

fn write_file(ws: &Path, name: &str, contents: &str) -> Result<()> {
    let path = ws.join(name);
    std::fs::write(&path, contents)
        .map_err(|e| GolemError::Io(format!("write {}: {e}", path.display())))
}

/// Count `.png` files in `dir` (0 if it doesn't exist).
pub fn count_pngs(dir: &Path) -> usize {
    match std::fs::read_dir(dir) {
        Ok(read) => read
            .flatten()
            .filter(|e| {
                e.path()
                    .extension()
                    .and_then(|x| x.to_str())
                    .map(|x| x.eq_ignore_ascii_case("png"))
                    .unwrap_or(false)
            })
            .count(),
        Err(_) => 0,
    }
}

/// A reviewer's verdict (parsed leniently from its JSON artifact).
pub struct Verdict {
    pub complete: bool,
    pub feedback: String,
}

pub fn read_verdict(path: &Path) -> Verdict {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => {
            return Verdict {
                complete: false,
                feedback: "reviewer did not write a verdict file".into(),
            };
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            return Verdict {
                complete: false,
                feedback: "reviewer verdict was not valid JSON".into(),
            };
        }
    };
    let complete = value.get("complete").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut feedback = value
        .get("feedback")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    for key in ["unmet_items", "issues"] {
        if let Some(arr) = value.get(key).and_then(|v| v.as_array()) {
            for item in arr {
                if let Some(s) = item.as_str() {
                    feedback.push_str("\n- ");
                    feedback.push_str(s);
                }
            }
        }
    }
    Verdict { complete, feedback }
}

// ----- agent prompts ------------------------------------------------------

pub const SOLVE_PROMPT: &str = "Read INSTRUCTIONS.md in the current directory and \
complete the SPICE task it describes. Edit and test iteratively using the Docker \
container as instructed. Your deliverable is `solution.cir` in this directory, \
satisfying every item in rubric.json and running with no errors or warnings.";

pub fn instructions_md(container: &str) -> String {
    format!(
        "# SPICE task — solve with ngspice\n\n\
Files in THIS directory (also mounted at /work inside Docker container `{container}`):\n\
- `prompt.txt` — the task to satisfy\n\
- `rubric.json` — every item here MUST be satisfied\n\
- reference image/other files — consult these\n\
- `starting_state.txt` and any starting-state files (may be empty)\n\n\
## Running ngspice (inside the container)\n\
Run ngspice on files in this directory via the container, e.g.:\n\
```\ndocker exec {container} ngspice -b /work/solution.cir\n```\n\
`/work` inside the container IS this directory. Use a `.control` block for \
analyses, or `-b` batch mode.\n\n\
## Your job\n\
1. Read `prompt.txt` and `rubric.json`. Then VIEW every reference image in this \
directory (the `.png` / image files, e.g. `reference_*.png`) using your Read tool — \
they show the required circuit topology / block diagram you must implement. Do not \
skip them.\n\
2. Write the final netlist to `solution.cir` here. Declare ALL device models \
INLINE. It MUST run with no errors and no warnings.\n\
3. Implement EVERY rubric item (DC operating point, transient ≥ 500ms showing the \
required behavior, the alarm logic, required output levels, etc.).\n\
4. Test by running ngspice in the container until it runs cleanly and the behavior \
matches the rubric. Use `wrdata` to dump signals for inspection.\n\
5. Re-run ngspice after every change. Do not stop until `solution.cir` runs \
cleanly and satisfies the rubric.\n"
    )
}

pub fn review_solution_prompt(container: &str) -> String {
    format!(
        "You are an INDEPENDENT reviewer. In the current directory: `prompt.txt` \
(the task), `rubric.json` (requirements), and `solution.cir` (the proposed \
netlist). Verify it yourself: run `docker exec {container} ngspice -b /work/solution.cir` \
and inspect the output. Check EVERY rubric item and that it runs with NO errors and \
NO warnings. Then write your verdict as JSON to `review_solve.json` with EXACTLY these \
fields: {{\"complete\": boolean, \"unmet_items\": [strings], \"feedback\": string}}. \
Only set complete=true if every rubric item is genuinely satisfied and the netlist \
runs cleanly. Be strict and specific in feedback."
    )
}

pub fn plot_prompt(container: &str) -> String {
    format!(
        "In the current directory, `solution.cir` is a verified ngspice netlist. Produce a \
PNG plot for every signal the rubric requires (e.g. input, rate-of-change node, \
threshold reference, qualification node, decision-stage output, final RAPID CHANGE \
output). Do everything INSIDE the container via `docker exec {container} ...`; first run \
`docker exec {container} mkdir -p /work/plots`.\n\n\
Create the plotting deck `plot.cir` by COPYING `solution.cir` and changing ONLY its \
`.control` block to emit plots (keep the same components, models, sources, and \
analyses — do not alter the circuit). This keeps the plots faithful to the submitted \
`solution.cir`.\n\n\
IMPORTANT — ngspice's `gnuplot` command does NOT emit a PNG by itself (it writes a \
`.plt` gnuplot script + a `.data` file). Use this exact TWO-STEP recipe per plot:\n\
1. In `plot.cir`'s `.control` block, run the analysis then \
`gnuplot plots/<name> <exprs>` (e.g. `gnuplot plots/roc v(roc) v(thresh)`). This writes \
`plots/<name>.plt` and `plots/<name>.data`.\n\
2. Render the PNG headlessly with gnuplot:\n\
`docker exec {container} sh -c \"cd /work && gnuplot -e \\\"set terminal pngcairo size 1200,700; set output 'plots/<name>.png'\\\" plots/<name>.plt\"`\n\n\
Confirm each `plots/<name>.png` exists and is a real PNG. Keep plotting in a separate \
deck so `solution.cir` stays clean. List the PNGs you produced."
    )
}

pub fn review_plots_prompt(_container: &str) -> String {
    "You are an INDEPENDENT reviewer with vision. In the current directory: \
`prompt.txt`, `rubric.json`, and PNG plots in `plots/`. Open and examine EACH plot \
image. Judge whether the plots demonstrate that the netlist satisfies the rubric's \
behavioral requirements (e.g. RAPID CHANGE asserts only after a qualified rapid-change \
interval, ignores short spikes, resets correctly, correct output voltage levels). Write \
your verdict as JSON to `review_plots.json` with EXACTLY: {\"complete\": boolean, \
\"issues\": [strings], \"feedback\": string}. Be strict."
        .to_string()
}
