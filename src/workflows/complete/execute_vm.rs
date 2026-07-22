//! "Execute on VM" — type a solved netlist into the Vagon VM's nvim at a human
//! pace, posting Vagon activity-log checkpoints at evenly-spaced points. Reuses
//! the same humanized typing PLAN as "Complete task" (rhythm / typos / `:w`
//! saves) and the mock-nvim self-test, but emits keystrokes as PHYSICAL key
//! events (scancodes) the remote-desktop stream forwards, not browser key
//! events. The VM is a pixel stream with no read-back, so the plan is verified by
//! simulation up front and then typed verbatim: `:set paste` (no autoindent /
//! autopairs) and `:set mouse=` (so the refocus click between checkpoints can't
//! move the cursor).

use std::time::Duration;

use crate::prelude::*;
use crate::workflows::feather::vagon_log;

use super::typing::{self, Action, TypingConfig};

pub struct ExecuteOnVm;

#[async_trait]
impl Workflow for ExecuteOnVm {
    fn name(&self) -> &'static str {
        "Execute on VM (type netlist + checkpoints)"
    }
    fn description(&self) -> &'static str {
        "Type the solved netlist into the Vagon VM's nvim at a human pace, with Vagon-log checkpoints."
    }
    fn inputs(&self) -> Vec<InputSpec> {
        vec![
            InputSpec::required("task_id", "Task id (the solved task to type)"),
            InputSpec::optional("source_file", "Netlist file in final/", "solution.cir"),
            InputSpec::optional("duration_minutes", "Target duration (minutes)", "180"),
            InputSpec::optional("seed", "Random seed (blank = random)", ""),
            InputSpec::optional("typos", "Simulate typos+corrections (true/false)", "false"),
        ]
    }

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        // --- load the (possibly operator-edited) netlist + checkpoints ---
        let id = ctx.require_input("task_id")?;
        let source = ctx
            .input("source_file")
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "solution.cir".to_string());
        let solve_dir = ctx.settings.output_dir.join("solve").join(&id);
        let netlist_path = solve_dir.join("final").join(&source);
        let text = std::fs::read_to_string(&netlist_path).map_err(|e| {
            GolemError::Io(format!("read netlist {}: {e}", netlist_path.display()))
        })?;
        if text.trim().is_empty() {
            return Err(ctx
                .stop_and_warn(format!("netlist {} is empty", netlist_path.display()))
                .await);
        }
        let checkpoints = load_checkpoints(&solve_dir.join("checkpoints.json"));
        ctx.output(format!("{} checkpoint(s) to post while typing", checkpoints.len()));

        // --- typing plan + self-test (no key is sent unless the plan reproduces
        //     the netlist exactly) ---
        let minutes: f64 = ctx
            .input("duration_minutes")
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|m| m.is_finite())
            .unwrap_or(180.0)
            .clamp(1.0, 24.0 * 60.0);
        let seed: u64 = ctx
            .input("seed")
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or_else(rand::random);
        let mut cfg = TypingConfig::default();
        if ctx.input("typos").map(|s| s.trim().eq_ignore_ascii_case("true")) == Some(true) {
            cfg.typo_chance = 0.012;
            cfg.long_typo_chance = 0.0025;
        }
        let target = Duration::from_secs_f64(minutes * 60.0);
        let plan = typing::generate(&text, target, &cfg, seed);
        ctx.output(format!(
            "plan: {} keystrokes over {} lines, {} saves, est {} (target {}, seed {seed})",
            plan.keystrokes,
            plan.lines,
            plan.saves,
            fmt_dur(plan.total),
            fmt_dur(target),
        ));

        ctx.step("self-test").await?;
        let (sim_text, _) = super::nvim::simulate(&plan.events);
        if sim_text.trim_end() != text.trim_end() {
            return Err(ctx
                .stop_and_warn(
                    "self-test FAILED: the simulated buffer does not match the netlist; aborting \
                     before typing anything.",
                )
                .await);
        }

        // --- attach to the Vagon tab + grab keyboard focus ---
        ctx.step("attach to the VM").await?;
        let wait = Duration::from_millis(ctx.settings.default_wait_timeout_ms);
        if !ctx
            .switch_to_target("app.vagon.io", "expired", wait.max(Duration::from_secs(30)))
            .await?
        {
            return Err(ctx
                .stop_and_warn(
                    "Could not find the Vagon session tab. Run 'Open VM terminal' first so the VM \
                     is connected and nvim is open.",
                )
                .await);
        }
        ctx.bring_to_front().await?;
        ctx.human_pause(500, 1000).await?;
        focus_stream(ctx).await?;

        // --- prepare nvim: paste mode (verbatim), mouse off (refocus click can't
        //     move the cursor), empty buffer ---
        ctx.step("prepare nvim").await?;
        ctx.press_key("Escape").await?;
        ctx.human_pause(150, 400).await?;
        type_vm(ctx, ":set paste mouse=").await?;
        ctx.press_key("Enter").await?;
        ctx.human_pause(200, 500).await?;
        ctx.press_key("Escape").await?;
        type_vm(ctx, ":%d").await?;
        ctx.press_key("Enter").await?;
        ctx.human_pause(300, 700).await?;

        // --- type the netlist, posting checkpoints at save boundaries ---
        ctx.step("type netlist").await?;
        let total_ms = plan.total.as_secs_f64() * 1000.0;
        let n = checkpoints.len();
        let mut next_cp = 0usize;
        let mut done_ms = 0.0_f64;
        let mut cur_line = 0usize;
        let mut saves = 0usize;
        let mut logs_written = 0usize;
        let mut last_status = String::new();
        let total_events = plan.events.len();

        for (i, ev) in plan.events.iter().enumerate() {
            ctx.guard().await?;
            emit_vm(ctx, ev.action).await?;
            if matches!(ev.action, Action::Enter) {
                cur_line += 1;
            }
            if matches!(ev.action, Action::CmdEnter) {
                saves += 1;
                // We are in NORMAL mode right after a `:w` save (the save group is
                // <Esc>:w<CR> then `i`), so a dock interaction + refocus here is
                // safe and leaves the VM state untouched. Post the next checkpoint
                // if its time threshold has passed.
                if next_cp < n {
                    let threshold = (next_cp + 1) as f64 / (n + 1) as f64 * total_ms;
                    if done_ms >= threshold {
                        if let Some(cp) = checkpoints.get(next_cp)
                            && post_checkpoint(ctx, cp, next_cp + 1, n).await
                        {
                            logs_written += 1;
                        }
                        next_cp += 1;
                    }
                }
            }
            // Live activity for the pipeline card + status line (throttled to
            // changes). The percentage bar comes from `progress` below; this is the
            // human-readable "what's happening right now" line.
            let here = (cur_line + 1).min(plan.lines);
            let activity = if matches!(ev.action, Action::CmdEnter) {
                format!("saving (:w) — {saves} saved")
            } else if matches!(ev.action, Action::Escape | Action::Key(_)) {
                "reviewing & fixing the netlist…".to_string()
            } else if ev.delay_after >= Duration::from_millis(1500) {
                format!("thinking ~{} (line {here}/{})", fmt_dur(ev.delay_after), plan.lines)
            } else {
                format!("typing line {here}/{}", plan.lines)
            };
            if activity != last_status {
                ctx.note_status(activity.clone());
                last_status = activity;
            }

            ctx.idle(ev.delay_after).await?;
            done_ms += ev.delay_after.as_secs_f64() * 1000.0;

            let frac = (i + 1) as f32 / total_events.max(1) as f32;
            let eta = ms_to_dur((plan.total.as_secs_f64() * 1000.0 - done_ms).max(0.0));
            ctx.progress(
                Some(frac),
                format!(
                    "line {}/{}, {saves} saves, {logs_written}/{n} logs, ~{} left",
                    cur_line.min(plan.lines),
                    plan.lines,
                    fmt_dur(eta)
                ),
            );
        }

        // Final save + any remaining checkpoints (typing ends in insert mode, so
        // drop to normal mode first).
        ctx.press_key("Escape").await?;
        ctx.human_pause(150, 400).await?;
        type_vm(ctx, ":w").await?;
        ctx.press_key("Enter").await?;
        ctx.human_pause(300, 700).await?;
        while next_cp < n {
            if let Some(cp) = checkpoints.get(next_cp)
                && post_checkpoint(ctx, cp, next_cp + 1, n).await
            {
                logs_written += 1;
            }
            next_cp += 1;
        }

        ctx.output(format!(
            "done typing the netlist on the VM — {logs_written}/{n} activity log(s) posted, {saves} saves"
        ));
        Ok(WorkflowOutcome::CompletedWith(json!({
            "task_id": id,
            "source": source,
            "keystrokes": plan.keystrokes,
            "saves": saves,
            "checkpoints": n,
            "logs_posted": logs_written,
            "duration": fmt_dur(plan.total),
        })))
    }
}

/// Post one Vagon checkpoint log, then re-grab keyboard focus. Returns whether
/// the log actually published. Failures are logged, NOT fatal — the netlist
/// typing matters more than a single log entry, and we must never pop a blocking
/// prompt in the automatic middle of the run.
async fn post_checkpoint(ctx: &mut WorkflowCtx, text: &str, k: usize, n: usize) -> bool {
    ctx.note_status(format!("checkpoint {k}/{n}: posting Vagon log"));
    ctx.output(format!("checkpoint {k}/{n}: \"{text}\""));
    let posted = match vagon_log::publish_log(ctx, text).await {
        Ok(()) => {
            ctx.output(format!("activity log {k}/{n} posted"));
            true
        }
        Err(e) => {
            ctx.warn(format!("checkpoint {k}/{n} log failed (continuing): {e}"));
            false
        }
    };
    // The dock interaction clicked browser DOM; re-grab the stream's keyboard.
    let _ = focus_stream(ctx).await;
    let _ = ctx.human_pause(400, 900).await;
    posted
}

/// Type a literal string into the VM as physical key events (scancodes).
async fn type_vm(ctx: &WorkflowCtx, s: &str) -> Result<()> {
    for c in s.chars() {
        ctx.guard().await?;
        ctx.send_char_vm(c).await?;
        ctx.human_pause(40, 120).await?;
    }
    Ok(())
}

/// Map one typing-plan Action onto VM keys (scancodes for chars, named keys for
/// Enter/Backspace/Escape). No dwell — the remote-desktop path has no held
/// variant; the inter-key timing carries the humanization.
async fn emit_vm(ctx: &WorkflowCtx, action: Action) -> Result<()> {
    match action {
        Action::EnterInsert => ctx.send_char_vm('i').await,
        Action::Char(c) | Action::Key(c) => ctx.send_char_vm(c).await,
        Action::Enter | Action::CmdEnter => ctx.press_key("Enter").await,
        Action::Backspace => ctx.press_key("Backspace").await,
        Action::Escape => ctx.press_key("Escape").await,
    }
}

/// Click an empty area of the stream (right of centre, clear of the bottom-left
/// dock) to grab keyboard capture. With `:set mouse=` this can't move the cursor.
async fn focus_stream(ctx: &mut WorkflowCtx) -> Result<()> {
    let vp = ctx.eval("[window.innerWidth, window.innerHeight]").await?;
    let vw = vp.as_array().and_then(|a| a.first()).and_then(Value::as_f64).unwrap_or(1280.0);
    let vh = vp.as_array().and_then(|a| a.get(1)).and_then(Value::as_f64).unwrap_or(720.0);
    ctx.click_at(vw * 0.78, vh * 0.4).await?;
    ctx.human_pause(300, 600).await
}

/// Load `checkpoints.json` (`{"checkpoints":[...]}` or a bare array). Missing or
/// unreadable → no checkpoints (the workflow just types).
fn load_checkpoints(path: &std::path::Path) -> Vec<String> {
    let Some(v) = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    else {
        return Vec::new();
    };
    let arr = if v.is_array() {
        v.as_array()
    } else {
        v.get("checkpoints").and_then(Value::as_array)
    };
    arr.map(|a| {
        a.iter()
            .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

fn ms_to_dur(v: f64) -> Duration {
    Duration::from_secs_f64((v / 1000.0).max(0.0))
}

fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{sec:02}s")
    } else {
        format!("{sec}s")
    }
}
