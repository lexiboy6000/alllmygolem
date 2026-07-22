//! "Complete task" — type a solved netlist into a browser-hosted neovim editor
//! using direct input (CDP keydown events, or native OS keys), at a human pace,
//! over the user's chosen duration.

use std::path::PathBuf;
use std::time::Duration;

use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

use crate::prelude::*;

use super::demo::NVIM_DEMO_HTML;
use super::typing::{self, Action, TypingConfig};

pub struct CompleteTask;

#[async_trait]
impl Workflow for CompleteTask {
    fn name(&self) -> &'static str {
        "Complete task (type netlist)"
    }
    fn description(&self) -> &'static str {
        "Type a solved netlist into a browser neovim editor at a human pace over a chosen duration."
    }
    fn inputs(&self) -> Vec<InputSpec> {
        vec![
            InputSpec::required("task_id", "Task id (the solved task to type)"),
            InputSpec::optional("source_file", "Netlist file in final/", "solution.cir"),
            InputSpec::optional("duration_minutes", "Target duration (minutes)", "60"),
            InputSpec::optional(
                "editor_url",
                "Editor URL (blank = built-in demo page; external editors are typed verbatim, unverified — disable autoindent/autopairs / use :set paste)",
                "",
            ),
            InputSpec::optional("editor_selector", "Editor element selector", "#editor"),
            InputSpec::optional("seed", "Random seed (blank = random)", ""),
            InputSpec::optional("typos", "Simulate typos+corrections (true/false)", "false"),
        ]
    }

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        // --- load the netlist ---
        let id = ctx.require_input("task_id")?;
        let source = ctx
            .input("source_file")
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "solution.cir".to_string());
        let netlist_path = ctx
            .settings
            .output_dir
            .join("solve")
            .join(&id)
            .join("final")
            .join(&source);
        let text = std::fs::read_to_string(&netlist_path).map_err(|e| {
            GolemError::Io(format!("read netlist {}: {e}", netlist_path.display()))
        })?;
        if text.trim().is_empty() {
            return Err(ctx
                .stop_and_warn(format!("netlist {} is empty", netlist_path.display()))
                .await);
        }

        // --- build the typing plan ---
        let minutes: f64 = ctx
            .input("duration_minutes")
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|m| m.is_finite()) // NaN/inf survive clamp and panic Duration::from_secs_f64
            .unwrap_or(60.0)
            .clamp(1.0, 24.0 * 60.0);
        let seed: u64 = ctx
            .input("seed")
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or_else(rand::random);
        let mut cfg = TypingConfig::default();
        if ctx.input("typos").map(|s| s.trim().eq_ignore_ascii_case("true")) == Some(true) {
            // ~1 short correction per ~80 chars; long-context (deferred) ones are
            // rarer since each one disrupts with a navigate-back.
            cfg.typo_chance = 0.012;
            cfg.long_typo_chance = 0.0025;
        }
        let target = Duration::from_secs_f64(minutes * 60.0);
        let plan = typing::generate(&text, target, &cfg, seed);
        ctx.output(format!(
            "plan: {} keystrokes over {} lines, {} saves, estimated duration {} (target {}, seed {seed})",
            plan.keystrokes,
            plan.lines,
            plan.saves,
            fmt_dur(plan.total),
            fmt_dur(target),
        ));
        // The plan can never type faster than human speed; if the chosen duration
        // is below that floor, the run overshoots the target. Warn so the user can
        // pick a longer duration instead of being surprised by a longer run.
        if plan.total.as_secs_f64() > target.as_secs_f64() * 1.2 {
            ctx.warn(format!(
                "{} keystrokes can't be typed in {} at a human pace; this run will take about {}. \
                 Increase 'duration_minutes' for a closer match.",
                plan.keystrokes,
                fmt_dur(target),
                fmt_dur(plan.total),
            ));
        }

        // --- self-test: dry-run the entire plan through the mock neovim (no
        // delays, no browser) and confirm it reproduces the netlist EXACTLY
        // before sending a single real keystroke. The typo/correction machinery
        // is intricate, so this guarantees we never type a wrong buffer. ---
        ctx.step("self-test").await?;
        let (sim_text, _sim_saves) = super::nvim::simulate(&plan.events);
        if sim_text.trim_end() != text.trim_end() {
            return Err(ctx
                .stop_and_warn(format!(
                    "self-test FAILED: the simulated buffer ({} chars) does not match the \
                     netlist ({} chars); aborting before typing anything.",
                    sim_text.chars().count(),
                    text.chars().count()
                ))
                .await);
        }
        ctx.output(format!(
            "self-test passed: {} events reproduce the netlist exactly",
            plan.events.len()
        ));

        // --- determine the target page ---
        let url = match ctx.input("editor_url").map(str::to_string).filter(|u| !u.trim().is_empty()) {
            Some(u) => u,
            None => {
                let path = ctx.settings.output_dir.join("nvim-demo.html");
                std::fs::write(&path, NVIM_DEMO_HTML)
                    .map_err(|e| GolemError::Io(format!("write demo page: {e}")))?;
                ctx.output("using built-in neovim demo page");
                format!("file://{}", absolute(&path).display())
            }
        };
        let is_demo = ctx
            .input("editor_url")
            .map(|u| u.trim().is_empty())
            .unwrap_or(true);
        let selector = ctx
            .input("editor_selector")
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "#editor".to_string());

        // External editors get the keystrokes verbatim but are NOT verified, and
        // editor-side magic (autoindent / autopairs / abbreviations) can change
        // what lands in the buffer. Warn so the user disables it first.
        if !is_demo {
            ctx.warn(
                "external editor: keystrokes are typed verbatim and NOT verified afterwards. \
                 Disable editor auto-formatting first — in neovim run `:set paste` (and/or turn off \
                 autoindent and any autopair/bracket plugins) or the typed text may differ from the netlist.",
            );
        }

        ctx.step("open editor").await?;
        ctx.navigate(&url).await?;
        ctx.wait_for_default("body").await?;
        ctx.human_pause(600, 1400).await?;
        ctx.output(format!("clicking editor ({selector})"));
        if let Err(e) = ctx.click(&selector).await {
            ctx.warn(format!("could not click '{selector}' ({e}); assuming the editor is focused"));
        }
        ctx.human_pause(400, 900).await?;

        // On a real editor the buffer may hold a template; the plan assumes an
        // empty buffer (the self-test verifies against empty), so clear it first
        // with `:%d`. The demo page starts empty and doesn't model `:%d`, so skip.
        if !is_demo {
            ctx.output("clearing the editor buffer (:%d)");
            ctx.press_key("Escape").await?;
            ctx.human_pause(120, 350).await?;
            for c in [':', '%', 'd'] {
                ctx.send_char(c).await?;
                ctx.human_pause(40, 130).await?;
            }
            ctx.press_key("Enter").await?;
            ctx.human_pause(200, 500).await?;
        }

        // --- execute the plan ---
        ctx.step("type netlist").await?;
        // Editor rect (for occasional mouse drift during long thinks) + a seeded
        // rng for dwell/mouse decisions (kept off the deterministic plan rng).
        let editor_rect = ctx.element_rect(&selector).await.ok().flatten();
        let mut hrng = StdRng::seed_from_u64(seed ^ 0x68756d_616e7a);
        let total_events = plan.events.len();
        let total_ms = plan.total.as_secs_f64() * 1000.0;
        let mut done_ms = 0.0_f64;
        let mut cur_line = 0usize;
        let mut saves_done = 0usize;
        let mut last_status = String::new();

        for (i, ev) in plan.events.iter().enumerate() {
            // Split the inter-key interval into a realistic key DWELL (key held
            // down) + the remaining FLIGHT (gap to the next key), so the total
            // timing is unchanged but the keydown->keyup hold isn't ~0.
            let interval = ev.delay_after;
            let dwell_ms = (interval.as_secs_f64() * 1000.0 * 0.5)
                .min(hrng.random_range(45.0..100.0))
                .max(0.0);
            let dwell = Duration::from_secs_f64(dwell_ms / 1000.0);

            match ev.action {
                Action::EnterInsert => ctx.send_char_held('i', dwell).await?,
                Action::Char(c) => ctx.send_char_held(c, dwell).await?,
                Action::Enter => {
                    ctx.press_key_held("Enter", dwell).await?;
                    cur_line += 1;
                }
                Action::Backspace => ctx.press_key_held("Backspace", dwell).await?,
                Action::Escape => ctx.press_key_held("Escape", dwell).await?,
                Action::Key(c) => ctx.send_char_held(c, dwell).await?,
                Action::CmdEnter => {
                    // Command-mode Enter runs `:w`; it is NOT a buffer newline,
                    // so it must not advance the line counter.
                    ctx.press_key_held("Enter", dwell).await?;
                    saves_done += 1;
                    ctx.output(format!("saved (:w) — {saves_done} so far"));
                }
            }

            // Progress + ETA from the pre-generated schedule.
            done_ms += ev.delay_after.as_secs_f64() * 1000.0;
            let frac = (i + 1) as f32 / total_events as f32;
            let remaining = ms_to_dur((total_ms - done_ms).max(0.0));
            ctx.progress(
                Some(frac),
                format!(
                    "line {}/{}, {} saves, ~{} left",
                    cur_line.min(plan.lines),
                    plan.lines,
                    saves_done,
                    fmt_dur(remaining)
                ),
            );

            // Live activity shown on the Golem page (the blue "Running" line).
            // Throttled: only emit when the phrase changes.
            let activity = if matches!(ev.action, Action::CmdEnter) {
                format!("saving (:w) — {saves_done} saved")
            } else if matches!(ev.action, Action::Escape | Action::Key(_)) {
                "reviewing & fixing the netlist…".to_string()
            } else if ev.delay_after >= Duration::from_millis(1500) {
                let cur = (cur_line + 1).min(plan.lines);
                format!("thinking ~{} (line {cur}/{})", fmt_dur(ev.delay_after), plan.lines)
            } else {
                let cur = (cur_line + 1).min(plan.lines);
                format!("typing line {cur}/{}", plan.lines)
            };
            if activity != last_status {
                ctx.note_status(activity.clone());
                if ev.delay_after >= Duration::from_secs(4) && matches!(ev.action, Action::Enter) {
                    ctx.output(format!("pausing to think for ~{}", fmt_dur(ev.delay_after)));
                }
                last_status = activity;
            }

            // The remaining gap after the key dwell. During a long "think" the
            // mouse occasionally drifts to a random spot over the editor (a human
            // doesn't keep a pixel-perfectly still cursor for minutes). A bare
            // move never moves the vim cursor, so the buffer is unaffected.
            let flight = interval.saturating_sub(dwell);
            if flight >= Duration::from_secs(3)
                && let Some(rect) = editor_rect
                && hrng.random_bool(0.4)
            {
                ctx.idle(flight.mul_f64(0.5)).await?;
                let x = rect.x + hrng.random_range(0.1..0.9) * rect.width;
                let y = rect.y + hrng.random_range(0.1..0.9) * rect.height;
                let _ = ctx.move_to(Point::new(x, y)).await; // best-effort drift
                ctx.idle(flight.mul_f64(0.5)).await?;
            } else {
                ctx.idle(flight).await?;
            }
        }

        // --- verify (demo only) ---
        if is_demo {
            ctx.step("verify").await?;
            let got = ctx
                .eval("window.__nvim ? window.__nvim.content : null")
                .await
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            let saves = ctx
                .eval("window.__nvim ? window.__nvim.saves : 0")
                .await
                .ok()
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            if got.trim_end() == text.trim_end() {
                ctx.output(format!("VERIFIED: editor content matches the netlist ({saves} saves)"));
            } else {
                ctx.warn(format!(
                    "editor content does NOT exactly match (got {} chars, expected {}); {saves} saves",
                    got.chars().count(),
                    text.chars().count()
                ));
            }
        }

        ctx.output("done typing the netlist");
        Ok(WorkflowOutcome::CompletedWith(json!({
            "task_id": id,
            "source": source,
            "keystrokes": plan.keystrokes,
            "saves": saves_done,
            "duration": fmt_dur(plan.total),
        })))
    }
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

/// Absolute path for a (possibly relative) path, without the Windows verbatim
/// prefix. (Mirrors solve::util::absolute to avoid a cross-module dependency.)
fn absolute(path: &std::path::Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|c| c.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}
