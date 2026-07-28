//! Small helpers shared by the task1/task_data/responseA/responseB workflows
//! (create_task1.rs, task_data.rs, create_response_a_dir.rs,
//! download_response_a.rs, download_response_b.rs).
//!
//! IMPORTANT Golem quirk this file works around: when workflows are chained
//! via `dependencies()`, the engine builds a BRAND NEW `WorkflowCtx` for each
//! one (see `src/engine/chain.rs`) -- so `ctx.set(...)` in one workflow is
//! NOT visible via `ctx.get(...)` in the next, and the shared `inputs` map is
//! fixed before the chain even starts (it can't hold a value one workflow
//! decides partway through, like an auto-incremented folder name). So the
//! task folder name is decided ONCE by "1. Create task1 directory"
//! (`resolve_or_create_task_dir`: task1, task2, task3, ... auto-incrementing
//! by checking what already exists in the output dir) and written to a
//! marker file on disk; every other workflow reads it back
//! (`current_task_dir`). A file is the one thing that reliably crosses the
//! fresh-ctx-per-workflow boundary.

use rand::RngExt;
use serde::Deserialize;

use crate::prelude::*;

/// Quote a Rust string as a safe JS string literal (e.g. `a` -> `"a"`).
fn js_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// Gaussian landing jitter for a click target: the finder JS returns a
/// control's exact centre, and clicking that same pixel on every selection is
/// a strong automation tell. Offsets the point with N(0, σ²) per axis
/// (σ from Settings > humanize jitter, clamped ±5px -- safe for the smallest
/// Good/Bad buttons). The verify-and-retry in `click_until_selected` catches
/// the (rare) case where even that lands off the control.
pub fn jittered(ctx: &WorkflowCtx, x: f64, y: f64) -> (f64, f64) {
    let mut rng = rand::rng();
    let (jx, jy) = crate::humanize::click_landing_jitter(&ctx.settings.humanize, &mut rng);
    (x + jx, y + jy)
}

// ----- automatic mode ----------------------------------------------------

/// Whether the pipeline's human gates should self-answer instead of blocking.
///
/// Every consultation of `settings.auto_mode` lives in this module and its
/// callers inside `first_test`, which is what scopes the flag to the task
/// pipeline: workflows 1-8 and any subworkflow they pull in honour it, while
/// the feather/complete/solve families keep their own prompts either way.
pub fn auto_mode(ctx: &WorkflowCtx) -> bool {
    ctx.settings.auto_mode
}

/// A [`WorkflowCtx::warn_user`] that never blocks in automatic mode.
///
/// These prompts all mean "Golem couldn't drive one control -- fix it on the
/// page and dismiss this". Automatic mode has nobody to do that, so the
/// message degrades to a warning line and the run continues to whatever check
/// would have caught the failure anyway (the `on_new_task` test, the submit
/// button's own enabled-check, and so on).
pub async fn warn_user_unless_auto(ctx: &WorkflowCtx, msg: impl Into<String>) -> Result<()> {
    let msg = msg.into();
    if auto_mode(ctx) {
        ctx.warn(format!("automatic mode, not waiting for a human: {msg}"));
        return Ok(());
    }
    ctx.warn_user(msg).await
}

/// A [`WorkflowCtx::stop_and_warn`] that doesn't wait for an acknowledgement in
/// automatic mode.
///
/// The chain halts either way -- the difference is only whether Golem sits on
/// an undismissed dialog first. Unattended, that turns a clean stop into a
/// wedge that holds the browser and the claimed task open, so automatic mode
/// halts immediately instead.
pub async fn halt_unless_auto(ctx: &WorkflowCtx, msg: impl Into<String>) -> GolemError {
    let msg = msg.into();
    if auto_mode(ctx) {
        return ctx.halt(msg);
    }
    ctx.stop_and_warn(msg).await
}

// ----- step 7: ask Claude, then click its answers in ----------------------

/// One criterion's judgment, as Claude is instructed to write it to the
/// `claude_answers` file (see `ANSWER_CRITERIA_PROMPT`).
#[derive(Deserialize)]
pub struct CriterionAnswer {
    pub number: u32,
    pub response_a: String,
    pub response_b: String,
    #[serde(default)]
    pub notes: String,
}

/// The page's separate "Overall Quality" pick ("Which generation better
/// fulfills the task requirements?" -> Response A / Response B / Tie).
#[derive(Deserialize)]
pub struct OverallAnswer {
    /// Exactly "Response A", "Response B", or "Tie" (matches the page's button text).
    pub winner: String,
    #[serde(default)]
    pub notes: String,
}

/// The full `claude_answers` file: per-criterion judgments plus the overall pick.
/// `criteria` defaults to empty: some tasks have no Evaluation Criteria list
/// at all and only ask for the overall pick.
#[derive(Deserialize)]
pub struct ClaudeAnswers {
    #[serde(default)]
    pub criteria: Vec<CriterionAnswer>,
    pub overall: OverallAnswer,
}

/// Run `claude` (the Claude Code CLI) as a subprocess, cwd'd into `task_dir`,
/// and have it read everything under task1 and write its judgments to
/// `task_dir/claude_answers`. Reuses the same launcher settings as the Solve
/// pipeline (Settings > Claude path / model / effort / timeout) -- this isn't
/// a "solve" workflow, but those settings are general-purpose, not solve-only.
///
/// `reviewer_feedback`, when given, is a human reviewer's note on what to fix
/// or reconsider: it is appended to the grading prompt and Claude is told to
/// revise the existing `claude_answers` accordingly.
pub async fn ask_claude_for_answers(
    ctx: &WorkflowCtx,
    task_dir: &std::path::Path,
    reviewer_feedback: Option<&str>,
) -> Result<()> {
    let claude = if ctx.settings.claude_path.trim().is_empty() {
        "claude".to_string()
    } else {
        ctx.settings.claude_path.clone()
    };
    let model = ctx.settings.solve_model.clone();
    let effort = ctx.settings.solve_effort.clone();
    let prompt: String = match reviewer_feedback {
        Some(fb) => format!(
            "{ANSWER_CRITERIA_PROMPT}\n\nIMPORTANT -- a human reviewer looked at the answers you \
             previously wrote to claude_answers and asks for a revision:\n\n\"{fb}\"\n\nRe-read \
             whatever is needed to address this, then REWRITE claude_answers completely (same \
             JSON shape). Keep judgments the reviewer didn't question unless re-reading changes \
             your mind."
        ),
        None => ANSWER_CRITERIA_PROMPT.to_string(),
    };
    let mut args: Vec<&str> = vec![
        "-p",
        prompt.as_str(),
        "--dangerously-skip-permissions",
        "--output-format",
        "stream-json",
        "--verbose",
    ];
    if !model.trim().is_empty() {
        args.push("--model");
        args.push(model.as_str());
    }
    if !effort.trim().is_empty() {
        args.push("--effort");
        args.push(effort.as_str());
    }
    let timeout = Duration::from_secs(ctx.settings.claude_timeout_secs.max(60));
    let out = ctx.run_claude(&claude, &args, Some(task_dir), Some(timeout)).await?;
    if !out.success() {
        return Err(GolemError::Other(format!(
            "claude exited with an error: {}",
            out.combined().trim()
        )));
    }
    Ok(())
}

/// Read + parse `claude_answers` written by `ask_claude_for_answers`.
pub fn read_claude_answers(path: &std::path::Path) -> Result<ClaudeAnswers> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| GolemError::Io(format!("read {}: {e}", path.display())))?;
    serde_json::from_str(&text).map_err(|e| {
        GolemError::Other(format!(
            "parse {}: {e} (claude may not have written the exact JSON shape asked for)",
            path.display()
        ))
    })
}

/// Apply a full set of Claude answers onto the multimango page: bring the
/// tab to the front (the clicks drive the real cursor), rate every criterion
/// for both responses with human pacing, then make the Overall pick.
/// Returns `(applied, missed-labels)`. Shared by workflow 7 and the review
/// workflow's "reviewer asked for changes -> re-apply" path.
///
/// `paced` (workflow 7's first application only) additionally inserts a
/// ~2-minute jittered break after every few selections -- see
/// [`long_answer_break`]. Pacing is PURELY timing: every answer value still
/// comes verbatim from `claude_answers`, each label keeps its own verdict
/// through the click-order shuffle below, and every click re-finds and
/// verifies its exact target (`click_until_selected`) exactly as before --
/// so slower never means less accurate. The re-apply paths pass `false`:
/// they run while a human reviewer waits, or right before submit.
pub async fn apply_answers(
    ctx: &mut WorkflowCtx,
    answers: &ClaudeAnswers,
    paced: bool,
) -> Result<(usize, Vec<String>)> {
    // The clicks drive the real OS cursor, so the task page has to actually
    // be the visible tab -- a native click lands on whatever is on screen.
    if let Err(e) = ctx.browser.bring_to_front().await {
        ctx.warn(format!(
            "couldn't bring the task tab to the front ({e}) -- continuing anyway"
        ));
    }
    let mut applied = 0usize;
    let mut missed: Vec<String> = Vec::new();
    // Long-break pacing: selections left before the next ~2-minute break.
    // u32::MAX effectively disables it on the unpaced paths.
    let mut until_break: u32 = if paced { next_batch_size() } else { u32::MAX };
    for (i, a) in answers.criteria.iter().enumerate() {
        // scope the (non-Send) thread rng so it never crosses an await
        let (b_first, reread, hesitate, wander) = {
            let mut rng = rand::rng();
            (
                rng.random_bool(0.5),
                rng.random_bool(0.2),
                rng.random_bool(0.15),
                rng.random_bool(0.35),
            )
        };
        if i > 0 {
            // The long break fires at row boundaries only -- a person
            // finishes the row they're reading before stepping away.
            if until_break == 0 {
                long_answer_break(ctx).await?;
                until_break = next_batch_size();
            }
            // a beat to "read" the next criterion before rating it
            ctx.human_pause(700, 2600).await?;
            if wander {
                // idle mouse drift while "reading" -- moves only, no clicks
                ctx.wander_cursor().await?;
            }
            if reread {
                // sometimes a person goes back and re-reads the rubric line
                ctx.human_pause(1500, 4200).await?;
            }
        }
        // a real eye doesn't always sweep left-to-right: on some rows rate
        // Response B before Response A. Only the CLICK ORDER flips -- each
        // label keeps its own verdict, so the ratings applied are unchanged.
        let mut pair = [
            ("Response A", a.response_a.as_str()),
            ("Response B", a.response_b.as_str()),
        ];
        if b_first {
            pair.swap(0, 1);
        }
        for (j, (label, want)) in pair.into_iter().enumerate() {
            if j > 0 && hesitate {
                // occasional second-guessing pause before the second pick
                ctx.human_pause(1200, 3000).await?;
            }
            let ok = click_criterion_button(ctx, a.number, label, want).await?;
            until_break = until_break.saturating_sub(1);
            if ok {
                applied += 1;
                // dwell between individual selections like a person deciding
                ctx.human_pause(350, 1900).await?;
            } else {
                missed.push(format!("#{} {} -> {}", a.number, label, want));
            }
        }
    }
    // the overall pick is the last "answer" -- it can land after a break too
    if until_break == 0 {
        long_answer_break(ctx).await?;
    }
    // weighing the overall verdict takes longer than a single row
    ctx.human_pause(1000, 3600).await?;
    let wander = {
        let mut rng = rand::rng();
        rng.random_bool(0.35)
    };
    if wander {
        ctx.wander_cursor().await?;
    }
    if click_overall_button(ctx, &answers.overall.winner).await? {
        applied += 1;
        ctx.output(format!("overall quality: {}", answers.overall.winner));
    } else {
        missed.push(format!("overall quality -> {}", answers.overall.winner));
    }
    Ok((applied, missed))
}

/// How many individual selections to make before the next long break.
fn next_batch_size() -> u32 {
    let mut rng = rand::rng();
    rng.random_range(3..=7)
}

/// A long "stepped away / reading carefully" break between batches of
/// selections: near two minutes, jittered (85-155s target), slept in small
/// humanized chunks -- each chunk is a `human_pause`, so Stop/Pause stay
/// responsive throughout and the humanize layer adds its own jitter on top
/// -- with occasional idle cursor drift. Timing only: it never touches which
/// button gets clicked.
async fn long_answer_break(ctx: &mut WorkflowCtx) -> Result<()> {
    let target_ms: u64 = {
        let mut rng = rand::rng();
        rng.random_range(85_000..=155_000)
    };
    ctx.output(format!(
        "pacing: taking a ~{}s break before the next batch of answers",
        target_ms / 1000
    ));
    let start = tokio::time::Instant::now();
    while start.elapsed() < Duration::from_millis(target_ms) {
        ctx.human_pause(2500, 6000).await?;
        // scope the (non-Send) thread rng so it never crosses an await
        let wander = {
            let mut rng = rand::rng();
            rng.random_bool(0.12)
        };
        if wander {
            ctx.wander_cursor().await?;
        }
    }
    Ok(())
}

/// Re-check (without clicking) that every answer still shows as selected on
/// the page. The platform can swap the open task under us -- submitting
/// after a swap would rate a DIFFERENT task -- so the review workflow runs
/// this immediately before the multimango submit. Returns the mismatches.
pub async fn verify_answers_applied(
    ctx: &mut WorkflowCtx,
    answers: &ClaudeAnswers,
) -> Result<Vec<String>> {
    let mut wrong = Vec::new();
    for a in &answers.criteria {
        for (label, want) in [
            ("Response A", a.response_a.as_str()),
            ("Response B", a.response_b.as_str()),
        ] {
            let js = CLICK_CRITERION_BUTTON_JS
                .replace("__NUM__", &js_str(&format!("{}.", a.number)))
                .replace("__RESP__", &js_str(label))
                .replace("__WANT__", &js_str(want));
            let v = ctx.eval(&js).await?;
            if !v.get("selected").and_then(Value::as_bool).unwrap_or(false) {
                wrong.push(format!("#{} {} -> {}", a.number, label, want));
            }
        }
    }
    let js = CLICK_OVERALL_BUTTON_JS.replace("__WANT__", &js_str(&answers.overall.winner));
    let v = ctx.eval(&js).await?;
    if !v.get("selected").and_then(Value::as_bool).unwrap_or(false) {
        wrong.push(format!("overall -> {}", answers.overall.winner));
    }
    Ok(wrong)
}

/// Find and click the `want` ("Good"/"Bad") button for `response_label`
/// ("Response A"/"Response B") in the row numbered `number` under the
/// Evaluation Criteria list. Returns `Ok(false)` if that exact button
/// couldn't be found (page structure differs, or the row/label doesn't
/// exist) rather than clicking something wrong.
///
/// These three appliers drive the REAL OS cursor (`click_at_cursor`, which
/// falls back to CDP if native input is unavailable) so the selections look
/// like a person working through the form, not events firing in a page
/// nobody is touching.
pub async fn click_criterion_button(
    ctx: &mut WorkflowCtx,
    number: u32,
    response_label: &str,
    want: &str,
) -> Result<bool> {
    let js = CLICK_CRITERION_BUTTON_JS
        .replace("__NUM__", &js_str(&format!("{number}.")))
        .replace("__RESP__", &js_str(response_label))
        .replace("__WANT__", &js_str(want));
    click_until_selected(ctx, &js).await
}

/// Find and click the Overall Quality button matching `want` ("Response A",
/// "Response B", or "Tie"), in the card headed by an `<h3>Overall Quality</h3>`.
pub async fn click_overall_button(ctx: &mut WorkflowCtx, want: &str) -> Result<bool> {
    let js = CLICK_OVERALL_BUTTON_JS.replace("__WANT__", &js_str(want));
    click_until_selected(ctx, &js).await
}

/// Click `find_js`'s target until its `selected` flag turns true. Each
/// finder computes `selected` from how its own widget renders the choice
/// (verified live on both): criterion Good/Bad buttons swap their neutral
/// `bg-background` classes for a filled color (`bg-emerald-600 text-white`
/// for Good), while the Overall buttons keep their classes and gain an
/// inline `background-color` style instead.
///
/// Cursor clicks can miss: the target's coordinates are read BEFORE the
/// humanized mouse travel (~0.5-1.5s), and this SPA re-renders every second
/// (task timer), which can reset the scroll of the containers around the
/// list in between -- the same race that hit the Task Data Download button.
/// So: re-find fresh coordinates before every press, verify the selection
/// actually registered after each, and after two missed cursor attempts fall
/// back to a CDP click (the pre-cursor mechanism that never missed). The
/// second cursor attempt starts with the cursor already next to the target,
/// so its lookup->press window is tiny and usually lands.
pub async fn click_until_selected(ctx: &mut WorkflowCtx, find_js: &str) -> Result<bool> {
    const ATTEMPTS: usize = 3;
    for attempt in 1..=ATTEMPTS {
        let v = ctx.eval(find_js).await?;
        let (x, y) = match (
            v.get("x").and_then(Value::as_f64),
            v.get("y").and_then(Value::as_f64),
        ) {
            (Some(x), Some(y)) => (x, y),
            _ => return Ok(false),
        };
        if v.get("selected").and_then(Value::as_bool).unwrap_or(false) {
            return Ok(true);
        }
        match attempt {
            1 => {}
            a if a < ATTEMPTS => {
                ctx.output("selection didn't register -- clicking again at fresh coordinates")
            }
            _ => ctx.warn("cursor clicks didn't register -- falling back to a CDP click"),
        }
        let (x, y) = jittered(ctx, x, y);
        if attempt < ATTEMPTS {
            ctx.click_at_cursor(x, y).await?;
        } else {
            ctx.click_at(x, y).await?;
        }
        // give React a beat to repaint the button state before verifying
        ctx.human_pause(300, 600).await?;
    }
    let v = ctx.eval(find_js).await?;
    Ok(v.get("selected").and_then(Value::as_bool).unwrap_or(false))
}

/// Find the Submit button (identified by being next to the "Skip" button)
/// and click it IF it's currently enabled. Returns `Ok(false)` if it's
/// missing or still disabled (e.g. not every criterion got rated), or if
/// even repeated clicks left it sitting there.
///
/// Verified like the selections above, but the success signal is inverted:
/// a submit that landed makes the enabled button GO AWAY (the page submits /
/// navigates), so "still findable after the click" means the click missed --
/// re-find fresh coordinates and try again, ending with a CDP click. No
/// double-submit risk: a click that actually submitted removes the button,
/// which ends the loop before another press.
pub async fn click_submit_if_enabled(ctx: &mut WorkflowCtx) -> Result<bool> {
    click_submit_with(ctx, FIND_SUBMIT_JS).await
}

/// The mechanism behind [`click_submit_if_enabled`], reusable with any
/// find-JS returning `{x, y}` for an ENABLED submit control (the review
/// workflow feeds it the Handshake page's submit button).
///
/// The button going away is the only evidence we have that a click landed,
/// and the page is not always quick to give it: Handshake's "Confirm time"
/// can take several seconds to process, with the button sitting there the
/// whole time. This used to wait a flat ~1.5s after each click and check
/// once, so a slow-but-perfectly-successful confirm read as a miss and the
/// run stopped on a "click it by hand" prompt. Poll for the disappearance
/// instead, and only re-click once the button has genuinely outlasted that
/// wait -- clicking again early is the actual danger here, since it aims a
/// second press at a page that already accepted the first.
pub async fn click_submit_with(ctx: &mut WorkflowCtx, find_js: &str) -> Result<bool> {
    const ATTEMPTS: usize = 3;
    /// How long the page gets to react before a click counts as missed.
    /// Deliberately generous: the two outcomes here are not symmetric. Waiting
    /// too long on a click that missed costs seconds, while re-clicking too
    /// early aims a second submission at a page still chewing on the first,
    /// and the button stays visible throughout that processing.
    const SETTLE: Duration = Duration::from_secs(15);
    /// The final attempt waits longer still: there is nothing after it but a
    /// prompt that stops the run dead, so patience is cheaper than giving up.
    const FINAL_SETTLE: Duration = Duration::from_secs(30);

    let mut clicked = false;
    for attempt in 1..=ATTEMPTS {
        let v = ctx.eval(find_js).await?;
        let (x, y) = match (
            v.get("x").and_then(Value::as_f64),
            v.get("y").and_then(Value::as_f64),
        ) {
            (Some(x), Some(y)) => (x, y),
            // Not findable (any more): either it was never enabled, or our
            // click just went through and the page moved on.
            _ => return Ok(clicked),
        };
        if clicked {
            ctx.output("Submit is still on screen -- the click may have missed; trying again");
        }
        let (x, y) = jittered(ctx, x, y);
        if attempt < ATTEMPTS {
            ctx.click_at_cursor(x, y).await?;
        } else {
            ctx.click_at(x, y).await?;
        }
        clicked = true;
        let patience = if attempt == ATTEMPTS { FINAL_SETTLE } else { SETTLE };
        if wait_until_gone(ctx, find_js, patience).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Poll `find_js` until it stops finding its target, giving up after
/// `patience`. `true` means it went away (the click registered).
///
/// Stop/Pause-aware via `human_pause`, and the pause comes first so a click
/// always gets a beat to dispatch before the first look.
async fn wait_until_gone(
    ctx: &mut WorkflowCtx,
    find_js: &str,
    patience: Duration,
) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + patience;
    loop {
        ctx.human_pause(350, 650).await?;
        if ctx.eval(find_js).await?.get("x").is_none() {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
    }
}

/// Recovery shared by steps 4 and 5: when a response's zip can't be found,
/// downloaded, or unzipped, the task can't be evaluated -- so instead of
/// stranding the pipeline, click the task page's "Skip" button (bottom of the
/// page, next to Submit) and queue a fresh run of this chain's original
/// targets. Dependency resolution makes the queued chain start over at
/// "1. Create task1 directory", which picks a NEW taskN folder for whatever
/// task Skip loaded (the aborted task's partial folder is left on disk).
///
/// Always returns `Err`: the CURRENT chain must abort either way -- letting
/// steps 6-8 keep running would rate the freshly-loaded task against the
/// aborted task's files. The queued chain starts the moment this one
/// finishes; Stop discards it, so stopping still ends everything. The
/// `Result<WorkflowOutcome>` return type only exists so `run` can end with
/// `util::skip_and_restart(...).await`.
///
/// If Skip can't be found or clicked, NOTHING is queued (restarting onto the
/// same stuck task would loop forever) -- the human is warned instead.
pub async fn skip_and_restart(
    ctx: &mut WorkflowCtx,
    label: &str,
    cause: GolemError,
) -> Result<WorkflowOutcome> {
    // A user Stop is a stop, never a skip.
    if matches!(cause, GolemError::StoppedByUser) {
        return Err(cause);
    }
    ctx.warn(format!(
        "{label} couldn't be retrieved ({cause}) -- skipping this task and restarting the \
         chain at workflow 1"
    ));
    match click_skip(ctx).await {
        Ok(true) => {
            ctx.queue_chain(ctx.chain_targets(), ctx.inputs_snapshot());
            Err(ctx.halt(format!(
                "{label} couldn't be retrieved -- the task was skipped and the chain will \
                 restart at workflow 1 on the next task (the aborted task's folder stays on \
                 disk). Press Stop to cancel the queued restart."
            )))
        }
        Ok(false) => Err(halt_unless_auto(ctx, format!(
                "{label} couldn't be retrieved ({cause}), and the Skip button couldn't be \
                 found/clicked either -- NOT restarting (that would loop on this same task). \
                 Skip the task by hand, then re-run the pipeline."
            ))
            .await),
        Err(GolemError::StoppedByUser) => Err(GolemError::StoppedByUser),
        Err(e) => Err(halt_unless_auto(ctx, format!(
                "{label} couldn't be retrieved ({cause}), and clicking Skip failed too ({e}) \
                 -- NOT restarting. Skip the task by hand, then re-run the pipeline."
            ))
            .await),
    }
}

/// Find and click the task page's "Skip" button. Success can't be judged by
/// the button disappearing alone -- a successful skip loads the NEXT task,
/// which renders its own Skip button in the same spot -- so the page's task
/// identity (every iframe src + the task-description text, neither of which
/// the once-a-second timer re-render touches) is snapshotted first, and a
/// click counts once that identity changes or the button goes away. When the
/// identity can't be read at all (badly broken page), exactly ONE cursor
/// click is made blind rather than risking a second press landing on the
/// NEXT task's Skip button. Returns `Ok(false)` if no enabled Skip button
/// ever appeared, or repeated clicks visibly changed nothing.
async fn click_skip(ctx: &mut WorkflowCtx) -> Result<bool> {
    // The clicks drive the real OS cursor -- the tab must be visible.
    if let Err(e) = ctx.browser.bring_to_front().await {
        ctx.warn(format!(
            "couldn't bring the task tab to the front ({e}) -- continuing anyway"
        ));
    }
    // The SPA may still be rendering; poll briefly for the button.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        ctx.guard().await?;
        let v = ctx.eval(FIND_SKIP_JS).await?;
        if v.get("x").and_then(Value::as_f64).is_some() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        ctx.human_pause(250, 400).await?;
    }
    let before = task_identity(ctx).await?;

    const ATTEMPTS: usize = 3;
    for attempt in 1..=ATTEMPTS {
        let v = ctx.eval(FIND_SKIP_JS).await?;
        let (x, y) = match (
            v.get("x").and_then(Value::as_f64),
            v.get("y").and_then(Value::as_f64),
        ) {
            (Some(x), Some(y)) => (x, y),
            // Gone between attempts: the previous click landed and the page
            // is mid-transition to the next task.
            _ => return Ok(true),
        };
        if attempt > 1 {
            ctx.output("Skip didn't register -- clicking again at fresh coordinates");
        }
        let (x, y) = jittered(ctx, x, y);
        if attempt < ATTEMPTS {
            ctx.click_at_cursor(x, y).await?;
        } else {
            ctx.click_at(x, y).await?;
        }
        // a skip takes a moment to go through / swap in the next task
        ctx.human_pause(900, 1600).await?;
        if before.is_empty() {
            return Ok(true);
        }
        if task_identity(ctx).await? != before {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The page's task identity for [`click_skip`]'s change detection.
async fn task_identity(ctx: &WorkflowCtx) -> Result<String> {
    let v = ctx.eval(TASK_IDENTITY_JS).await?;
    Ok(v.as_str().unwrap_or_default().to_string())
}

const ANSWER_CRITERIA_PROMPT: &str = "You are evaluating two AI-generated responses to a task, \
laid out in the current directory:\n\
- task_data/information -- the task description (what was asked for)\n\
- task_data/ -- reference files for the task, if any\n\
- task_data/evaluation_criteria/questions -- the numbered evaluation criteria, one per line \
(\"1. ...\", \"2. ...\", etc.)\n\
- responseA/ -- Response A's submitted files (read every file, especially the HTML deliverable)\n\
- responseB/ -- Response B's submitted files (read every file, especially the HTML deliverable)\n\n\
For EVERY criterion listed in task_data/evaluation_criteria/questions, judge Response A and \
Response B independently: does that response satisfy the criterion? Use \"Good\" if it does, \
\"Bad\" if it does not or only partially does.\n\n\
Separately, also give an OVERALL pick: which response, taken as a whole, better fulfills the \
task requirements -- \"Response A\", \"Response B\", or \"Tie\" if they're genuinely equal.\n\n\
Write your answer as a JSON OBJECT to a file named exactly `claude_answers` in the CURRENT \
directory (no file extension), in EXACTLY this shape:\n\
{\"criteria\": [{\"number\": 1, \"response_a\": \"Good\", \"response_b\": \"Bad\", \"notes\": \
\"one short sentence why\"}, ...], \"overall\": {\"winner\": \"Response A\", \"notes\": \"one \
short sentence why\"}}\n\
The \"criteria\" array must have one object per criterion, in the SAME order and using the SAME \
number as in the questions file. If the questions file is EMPTY or missing, this task has no \
per-criterion ratings: write \"criteria\": [] and judge only the overall pick. \
\"overall.winner\" must be EXACTLY one of \"Response A\", \"Response B\", or \"Tie\".\n\n\
Output ONLY that file -- do not print the JSON to stdout, do not add commentary elsewhere. Be \
strict and specific in your judgment.";

const CLICK_CRITERION_BUTTON_JS: &str = r#"(function(){
  var NUM = __NUM__;
  var RESP = __RESP__;
  var WANT = __WANT__;

  var spans = document.querySelectorAll('span');
  var header = null;
  for (var i = 0; i < spans.length; i++) {
    if ((spans[i].textContent || '').trim() === 'Evaluation Criteria') { header = spans[i]; break; }
  }
  if (!header) return null;
  var card = header.closest('[class*="rounded-lg"]') || header.parentElement;
  var list = card ? card.querySelector('.divide-y') : null;
  if (!list) return null;

  var rows = list.children;
  for (var j = 0; j < rows.length; j++) {
    var row = rows[j];
    var head = row.querySelector('.flex.gap-2') || row.firstElementChild;
    var kids = head ? head.children : [];
    var num = kids[0] ? (kids[0].textContent || '').trim() : '';
    if (num !== NUM) continue;

    var allDivs = row.querySelectorAll('div');
    for (var g = 0; g < allDivs.length; g++) {
      var grp = allDivs[g];
      var kids2 = grp.children;
      if (kids2.length < 3) continue;
      if (kids2[0].tagName !== 'SPAN') continue;
      if ((kids2[0].textContent || '').trim() !== RESP) continue;
      for (var b = 1; b < kids2.length; b++) {
        if (kids2[b].tagName === 'BUTTON' && (kids2[b].textContent || '').trim() === WANT) {
          var e = kids2[b];
          try { e.scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
          var r = e.getBoundingClientRect();
          if (r.width < 1 || r.height < 1) return null;
          return { x: r.left + r.width / 2, y: r.top + r.height / 2,
                   selected: ((e.className || '').toString().indexOf('bg-background') === -1) };
        }
      }
    }
    return null;
  }
  return null;
})()"#;

const CLICK_OVERALL_BUTTON_JS: &str = r#"(function(){
  var WANT = __WANT__;
  var h3s = document.querySelectorAll('h3');
  var header = null;
  for (var i = 0; i < h3s.length; i++) {
    if ((h3s[i].textContent || '').trim() === 'Overall Quality') { header = h3s[i]; break; }
  }
  if (!header) return null;
  var card = header.closest('[class*="rounded-lg"]') || header.parentElement;
  if (!card) return null;
  var btns = card.querySelectorAll('button');
  for (var j = 0; j < btns.length; j++) {
    if ((btns[j].textContent || '').trim() === WANT) {
      var e = btns[j];
      try { e.scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
      var r = e.getBoundingClientRect();
      if (r.width < 1 || r.height < 1) return null;
      /* Unlike the criterion buttons, the Overall buttons' CLASSES never
         change -- the pick is rendered via an inline style: selected gets a
         background-color (solid comparison color for A/B, muted fill for
         Tie), unselected has border/text colors only (verified live). */
      return { x: r.left + r.width / 2, y: r.top + r.height / 2,
               selected: ((e.getAttribute('style') || '').indexOf('background-color') !== -1) };
    }
  }
  return null;
})()"#;

const FIND_SUBMIT_JS: &str = r#"(function(){
  var btns = document.querySelectorAll('button');
  var skip = null;
  for (var i = 0; i < btns.length; i++) {
    if ((btns[i].textContent || '').trim().indexOf('Skip') !== -1) { skip = btns[i]; break; }
  }
  if (!skip || !skip.parentElement) return null;
  var siblings = skip.parentElement.querySelectorAll('button');
  for (var j = 0; j < siblings.length; j++) {
    var b = siblings[j];
    if (b === skip) continue;
    if (b.disabled || b.getAttribute('aria-disabled') === 'true') continue;
    try { b.scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
    var r = b.getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
  }
  return null;
})()"#;

/// The task page's enabled "Skip" button (the same one `FIND_SUBMIT_JS`
/// anchors on to find Submit), used to abandon a task whose deliverables the
/// site can't actually serve. The LAST visible match wins: if clicking Skip
/// pops a confirmation dialog with its own "Skip ..." button, that dialog
/// renders after (on top of) the page's button, so the retry click lands on
/// the confirmation instead of the covered original.
const FIND_SKIP_JS: &str = r#"(function(){
  var btns = document.querySelectorAll('button');
  var best = null;
  for (var i = 0; i < btns.length; i++) {
    var b = btns[i];
    if ((b.textContent || '').trim().indexOf('Skip') === -1) continue;
    if (b.disabled || b.getAttribute('aria-disabled') === 'true') continue;
    var r = b.getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    best = b;
  }
  if (!best) return null;
  try { best.scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
  var r = best.getBoundingClientRect();
  return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
})()"#;

/// A fingerprint of WHICH task the page is showing: every iframe's src (the
/// response iframes live on per-response hosts) plus the task-description
/// prose blocks' text. Both swap wholesale when Skip loads the next task,
/// and neither is touched by the SPA's once-a-second timer re-render.
const TASK_IDENTITY_JS: &str = r#"(function(){
  var parts = [];
  var ifr = document.querySelectorAll('iframe');
  for (var i = 0; i < ifr.length; i++) parts.push(ifr[i].getAttribute('src') || '');
  var prose = document.querySelectorAll('div.prose.prose-sm.max-w-none');
  for (var j = 0; j < prose.length; j++) parts.push((prose[j].innerText || '').slice(0, 500));
  return parts.join('|');
})()"#;

/// Whether `e` is the site saying the file simply is not there -- an HTTP 4xx
/// relayed by `curl -f` as `curl: (22) The requested URL returned error: 404`.
///
/// The distinction matters a lot, because the caller's response is to abandon
/// a real task. A timeout, DNS failure or dropped connection must NOT skip: it
/// says nothing about whether the deliverable exists, and skipping on one
/// would throw away a perfectly good task over a blip.
pub fn is_missing_file_error(e: &GolemError) -> bool {
    e.to_string()
        .split("returned error: ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|code| code.parse::<u16>().ok())
        .is_some_and(|code| (400..500).contains(&code))
}

/// Abandon the current multimango task and queue a fresh pipeline run.
///
/// For when a deliverable 404s: the task can never be completed, so clicking
/// "Skip" moves the site to the next one and the whole pipeline starts over
/// from workflow 1. Returns the error the caller should return, so the
/// current chain stops right here instead of carrying on through steps that
/// need files we don't have.
///
/// Queueing survives this chain failing -- only the user pressing Stop
/// discards a queued chain -- so the restart happens once the current run
/// unwinds. `rounds` is passed through UNCHANGED: a skipped task was never
/// completed and must not count against the total.
pub async fn skip_task_and_restart(ctx: &mut WorkflowCtx, reason: &str) -> GolemError {
    ctx.output(format!("{reason} -- skipping this task and starting over"));

    // The download steps run against multimango, but be explicit: a skip
    // aimed at the wrong tab would be a click into someone else's page.
    let timeout = Duration::from_millis(ctx.settings.default_wait_timeout_ms);
    let _ = ctx
        .browser
        .switch_to_target("multimango.com", "", timeout)
        .await;
    let _ = ctx.browser.bring_to_front().await;

    let mut skipped = false;
    match wait_for_skip(ctx, Duration::from_secs(15)).await {
        Ok(Some((x, y))) => {
            let (x, y) = jittered(ctx, x, y);
            match ctx.click_at(x, y).await {
                Ok(()) => {
                    skipped = true;
                    ctx.output("clicked Skip on the multimango task");
                    // Let the site load whatever comes next before the queued
                    // chain starts poking at the DOM.
                    let _ = ctx.human_pause(1500, 2500).await;
                }
                Err(e) => ctx.warn(format!("couldn't click Skip ({e})")),
            }
        }
        Ok(None) => ctx.warn("no enabled 'Skip' button found on the multimango page"),
        Err(e) => ctx.warn(format!("couldn't look for the Skip button ({e})")),
    }

    if !skipped {
        // Don't restart into the same broken task forever. Stop and let the
        // human decide -- this is the one outcome worth interrupting for.
        return ctx
            .stop_and_warn(format!(
                "{reason}, and the 'Skip' button couldn't be clicked. Skip the task by \
                 hand and re-run the pipeline."
            ))
            .await;
    }

    // Carry the pipeline's own inputs forward; task_dir stays empty so the
    // restart creates a fresh taskN rather than reusing the half-filled one.
    let mut inputs = std::collections::BTreeMap::new();
    inputs.insert("defer_submit".to_string(), "yes".to_string());
    if let Some(v) = ctx.input("target_minutes") {
        inputs.insert("target_minutes".to_string(), v.to_string());
    }
    if let Some(v) = ctx.input("rounds") {
        inputs.insert("rounds".to_string(), v.to_string());
    }
    ctx.queue_chain(vec![PIPELINE_WORKFLOW.to_string()], inputs);
    ctx.output("pipeline queued to start over from workflow 1 on the next task");

    GolemError::Halted(format!("{reason} -- task skipped, restarting"))
}

/// The pipeline's entry point, whose dependency chain is workflows 0 and 1-7.
const PIPELINE_WORKFLOW: &str = "8. Handshake review + submit (pipeline)";

/// Poll for an enabled Skip button, up to `timeout`.
async fn wait_for_skip(
    ctx: &mut WorkflowCtx,
    timeout: Duration,
) -> Result<Option<(f64, f64)>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        ctx.guard().await?;
        let v = ctx.eval(FIND_SKIP_JS).await?;
        if let (Some(x), Some(y)) = (
            v.get("x").and_then(Value::as_f64),
            v.get("y").and_then(Value::as_f64),
        ) {
            return Ok(Some((x, y)));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        ctx.human_pause(300, 600).await?;
    }
}

/// Where we record which task folder this run is using -- see
/// `resolve_or_create_task_dir` / `current_task_dir` below.
fn marker_path(ctx: &WorkflowCtx) -> std::path::PathBuf {
    ctx.settings.output_dir.join(".golem_current_task")
}

/// Scan `output_dir` for existing `task<N>` directories and return the next
/// unused number (1 if none exist yet).
fn next_task_number(output_dir: &std::path::Path) -> u32 {
    let mut max = 0u32;
    if let Ok(read) = std::fs::read_dir(output_dir) {
        for entry in read.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let raw_name = entry.file_name();
            let Some(name) = raw_name.to_str() else { continue };
            if let Some(num_str) = name.strip_prefix("task")
                && let Ok(num) = num_str.parse::<u32>()
            {
                max = max.max(num);
            }
        }
    }
    max + 1
}

/// Used ONLY by "1. Create task1 directory": decide the task folder name
/// (an explicit override typed into the `task_dir` field, else auto-
/// increment task1, task2, task3, ... by checking what already exists),
/// create it, and record the choice in a marker file.
///
/// Why a marker file and not `ctx.set`? Chained workflows each get a BRAND
/// NEW `WorkflowCtx` (see the module doc comment above) -- `ctx.set` in this
/// workflow would not be visible to the next one. A file on disk is the one
/// thing that reliably crosses that boundary.
pub fn resolve_or_create_task_dir(ctx: &WorkflowCtx) -> Result<std::path::PathBuf> {
    let output_dir = ctx.settings.output_dir.clone();
    let explicit = ctx.input("task_dir").map(str::trim).filter(|s| !s.is_empty());
    let name = match explicit {
        Some(n) => n.to_string(),
        None => format!("task{}", next_task_number(&output_dir)),
    };
    let dir = output_dir.join(&name);
    std::fs::create_dir_all(&dir)
        .map_err(|e| GolemError::Io(format!("mkdir {}: {e}", dir.display())))?;
    let marker = marker_path(ctx);
    std::fs::write(&marker, &name)
        .map_err(|e| GolemError::Io(format!("write {}: {e}", marker.display())))?;
    Ok(dir)
}

/// Used by every OTHER workflow in the family (steps 2-7): resolve the same
/// task folder step 1 picked for this run. An explicit `task_dir` field
/// still wins if you type one directly into THIS workflow's own field
/// (e.g. running it standalone); otherwise it reads the marker file step 1
/// wrote.
pub fn current_task_dir(ctx: &WorkflowCtx) -> Result<std::path::PathBuf> {
    let output_dir = &ctx.settings.output_dir;
    let explicit = ctx.input("task_dir").map(str::trim).filter(|s| !s.is_empty());
    if let Some(n) = explicit {
        return Ok(output_dir.join(n));
    }
    let marker = marker_path(ctx);
    let name = std::fs::read_to_string(&marker).map_err(|e| {
        GolemError::Io(format!(
            "read {}: {e} -- run \"1. Create task1 directory\" first (or type an explicit \
             folder name into this workflow's task_dir field) so this step knows which task \
             folder to use",
            marker.display()
        ))
    })?;
    Ok(output_dir.join(name.trim()))
}

/// The OS's real Downloads folder (e.g. `~/Downloads` on macOS) -- where
/// Chrome saves files when we DON'T redirect it, which is exactly what a
/// manual click does. The `bool` is whether the browser has to be *told* to
/// use it (see below).
///
/// Deliberately NOT `directories::UserDirs::download_dir()`. That follows the
/// XDG convention where a user directory pointing at `$HOME` means "disabled"
/// and returns `None` -- but a stock `xdg-user-dirs-update` on a fresh Arch /
/// Hyprland install writes exactly that (`XDG_DOWNLOAD_DIR="$HOME/"` for every
/// entry, when none of the folders existed at the time it ran). Golem then
/// failed with "couldn't determine the system Downloads folder" on a machine
/// where Chromium itself was saving downloads perfectly happily. So resolve it
/// the way Chromium does: `$XDG_DOWNLOAD_DIR`, then `user-dirs.dirs`, then
/// `~/Downloads`.
///
/// `$HOME` is never returned as the folder to WATCH. That resolution is legal
/// (and is what Chromium uses on such a machine), but this module moves the
/// new file it spots into the task folder, and `$HOME` gains unrelated files
/// all the time. When the lookup lands there, fall back to `~/Downloads` and
/// report `true` so the caller redirects the browser to match.
fn system_downloads_dir() -> Result<(std::path::PathBuf, bool)> {
    let home = directories::UserDirs::new()
        .map(|u| u.home_dir().to_path_buf())
        .or_else(|| std::env::var_os("HOME").map(std::path::PathBuf::from))
        .ok_or_else(|| GolemError::Other("couldn't determine the home directory".into()))?;

    let resolved = std::env::var_os("XDG_DOWNLOAD_DIR")
        .map(std::path::PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| xdg_download_dir_from_config(&home))
        .unwrap_or_else(|| home.join("Downloads"));

    // Normalize so a trailing separator ("$HOME/") compares equal to "$HOME".
    let resolved: std::path::PathBuf = resolved.components().collect();
    if resolved == home {
        return Ok((home.join("Downloads"), true));
    }
    Ok((resolved, false))
}

/// `XDG_DOWNLOAD_DIR` as written in `~/.config/user-dirs.dirs`, with a leading
/// `$HOME` expanded. This is the same file `xdg-user-dir DOWNLOAD` reads, and
/// what Chromium consults for its default download directory.
fn xdg_download_dir_from_config(home: &std::path::Path) -> Option<std::path::PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| home.join(".config"))
        .join("user-dirs.dirs");
    let text = std::fs::read_to_string(config).ok()?;
    for line in text.lines() {
        let Some(value) = line.trim().strip_prefix("XDG_DOWNLOAD_DIR=") else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        let path = match value.strip_prefix("$HOME") {
            Some(rest) => home.join(rest.trim_start_matches('/')),
            None => std::path::PathBuf::from(value),
        };
        if !path.as_os_str().is_empty() {
            return Some(path);
        }
    }
    None
}

/// Click a link (found via `find_js`, an IIFE returning `{x, y}` or `null`)
/// and wait for Chrome to actually save a new file into the OS's real
/// Downloads folder, then move it into `dest_dir/filename`.
///
/// We deliberately do NOT try to redirect Chrome's download folder via CDP
/// first (an earlier attempt at that didn't reliably work) -- instead this
/// lets Chrome do exactly what it already does for a normal manual click
/// (save into `~/Downloads`, auto-numbering `all_files (1).zip` etc. if a
/// name collides), and picks up whatever NEW file appears there as a result.
///
/// Detects "done downloading" by watching for a new filename to appear that
/// ISN'T a Chrome in-progress file (`.crdownload`/`.tmp`), then confirming
/// its size stops changing across a short pause.
///
/// The click itself is attempted up to 3 times within `timeout`, and the
/// target is re-located with `find_js` immediately before EVERY press. The
/// page is a live SPA that re-renders constantly (its task timer ticks every
/// second), and a re-render can reset the scroll of the containers around
/// the target between our lookup and the click, silently moving it out from
/// under the recorded coordinates -- observed live on the Task Data
/// "Download" button: Golem's click landed where the button used to be and
/// did nothing, while an identical CDP click at re-checked coordinates
/// downloaded immediately.
pub async fn click_and_wait_for_download(
    ctx: &mut WorkflowCtx,
    find_js: &str,
    dest_dir: &std::path::Path,
    filename: &str,
    timeout: Duration,
) -> Result<std::path::PathBuf> {
    std::fs::create_dir_all(dest_dir)
        .map_err(|e| GolemError::Io(format!("mkdir {}: {e}", dest_dir.display())))?;
    let (downloads, redirect) = system_downloads_dir()?;
    if redirect {
        // This machine's XDG config points the Downloads folder at $HOME, so
        // the browser would save into a directory we must not sweep for new
        // files. Send it somewhere dedicated instead.
        ctx.set_download_dir(&downloads).await?;
        ctx.output(format!(
            "system Downloads folder is configured as the home directory -- \
             pointing the browser at {} for this download",
            downloads.display()
        ));
    }
    let before = list_entries(&downloads);

    // The page can take a moment to render (SPA route change, data still
    // loading, etc.), so poll for the click target the same way the other
    // wait_for_* lookups do instead of giving up after a single eval. The
    // coordinates found here are deliberately thrown away -- the click loop
    // below re-reads them right before pressing.
    let find_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        ctx.guard().await?;
        let target = ctx.eval(find_js).await?;
        if target.get("x").and_then(Value::as_f64).is_some()
            && target.get("y").and_then(Value::as_f64).is_some()
        {
            break;
        }
        if tokio::time::Instant::now() >= find_deadline {
            return Err(GolemError::Other(
                "download link not found on the page".to_string(),
            ));
        }
        ctx.human_pause(250, 400).await?;
    }

    const MAX_CLICKS: usize = 3;
    let deadline = tokio::time::Instant::now() + timeout;
    let mut clicks_done = 0usize;
    // First click fires immediately; each later one only after the previous
    // click has had a few seconds to produce a file.
    let mut next_click_at = tokio::time::Instant::now();
    let downloaded = loop {
        ctx.guard().await?;
        let mut candidate: Option<(std::path::PathBuf, Option<std::time::SystemTime>)> = None;
        let mut in_progress = false;
        for (name, modified) in list_entries(&downloads) {
            let name_str = name.to_string_lossy();
            if name_str.ends_with(".crdownload") || name_str.ends_with(".tmp") {
                in_progress = true;
                continue;
            }
            let is_ours = match before.get(&name) {
                None => true,
                Some(previous) => previous != &modified,
            };
            // Several files can qualify (a stale overwrite plus a uniquified
            // `task-data (1).zip`); the newest is the one this click produced.
            if is_ours && candidate.as_ref().is_none_or(|(_, best)| modified > *best) {
                candidate = Some((downloads.join(&name), modified));
            }
        }
        let candidate = candidate.map(|(path, _)| path);
        if let Some(path) = candidate {
            // Confirm it's actually finished (stopped growing), not still mid-write.
            let size1 = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            ctx.human_pause(400, 700).await?;
            let size2 = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if size1 == size2 && size1 > 0 {
                break path;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(GolemError::Other(format!(
                "clicked the download control {clicks_done} time(s) but no new file \
                 appeared in {} within {}s",
                downloads.display(),
                timeout.as_secs()
            )));
        }
        // Re-click only while nothing is downloading -- a second click over an
        // active download would just fetch a duplicate.
        if !in_progress && clicks_done < MAX_CLICKS && tokio::time::Instant::now() >= next_click_at
        {
            let target = ctx.eval(find_js).await?;
            if let (Some(x), Some(y)) = (
                target.get("x").and_then(Value::as_f64),
                target.get("y").and_then(Value::as_f64),
            ) {
                if clicks_done > 0 {
                    ctx.output("no download started yet -- clicking again at fresh coordinates");
                }
                // Make the long humanized approach FIRST, then re-locate and
                // hop the short remaining distance -- the approach can take
                // over a second, plenty of time for a re-render to move the
                // target, so the final lookup->press window must stay small.
                ctx.move_to(Point::new(x, y)).await?;
                let fresh = ctx.eval(find_js).await?;
                let (fx, fy) = match (
                    fresh.get("x").and_then(Value::as_f64),
                    fresh.get("y").and_then(Value::as_f64),
                ) {
                    (Some(fx), Some(fy)) => (fx, fy),
                    _ => (x, y),
                };
                let (fx, fy) = jittered(ctx, fx, fy);
                ctx.click_at(fx, fy).await?;
                clicks_done += 1;
                next_click_at = tokio::time::Instant::now() + Duration::from_secs(8);
            }
        }
        ctx.human_pause(300, 500).await?;
    };

    let dest = dest_dir.join(filename);
    std::fs::rename(&downloaded, &dest).map_err(|e| {
        GolemError::Io(format!(
            "move {} -> {}: {e}",
            downloaded.display(),
            dest.display()
        ))
    })?;
    Ok(dest)
}

/// Every non-dotfile in `dir`, mapped to its last-modified time.
///
/// The modification time is load-bearing, not decoration: a download steered
/// by CDP `Browser.setDownloadBehavior` OVERWRITES a file of the same name
/// instead of uniquifying it to `task-data (1).zip` the way an ordinary Chrome
/// download would. Watching for a new *name* therefore never fires on the
/// second run onward -- the file lands correctly and the wait loop sits there
/// re-clicking until it times out. Comparing (name, mtime) catches both the
/// fresh name and the silent overwrite.
///
/// Dotfiles are excluded because whatever this scan picks gets MOVED, and a
/// shell or editor temp file appearing mid-wait must not be mistaken for the
/// payload.
fn list_entries(
    dir: &std::path::Path,
) -> std::collections::HashMap<std::ffi::OsString, Option<std::time::SystemTime>> {
    std::fs::read_dir(dir)
        .map(|read| {
            read.flatten()
                .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                .map(|e| {
                    let modified = e.metadata().and_then(|m| m.modified()).ok();
                    (e.file_name(), modified)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Finds the Task Data block's download control -- an `<a href>` link (older
/// layouts) or the archive-viewer's `<button>Download</button>` (newer
/// layout, see the block-finder comment above `TASK_DATA_ZIP_URL_JS`) -- and
/// returns its clickable centre `{x, y}`, for use with
/// `click_and_wait_for_download`.
///
/// The bare-anchor fallback (first `<a href>` in the block) is skipped when
/// the archive viewer is present: on that layout the description text itself
/// may contain ordinary links, and none of them is a download.
pub const TASK_DATA_ZIP_CLICK_JS: &str = r#"(function(){
  var all = document.querySelectorAll('div.prose.prose-sm.max-w-none');
  var box = null;
  for (var b = 0; b < all.length; b++) {
    var h = all[b].querySelector('h1,h2,h3');
    if (all[b].querySelector('[data-task-data-archive-viewer]') ||
        (h && /task\s*data/i.test(h.textContent || ''))) { box = all[b]; break; }
  }
  if (!box && all.length) box = all[0];
  if (!box) return null;
  var t = null;
  var as = box.querySelectorAll('a[href]');
  for (var i = 0; i < as.length; i++) {
    var href = as[i].getAttribute('href') || '';
    var txt = (as[i].textContent || '').toLowerCase();
    if (/\.zip(\?|$)/i.test(href) || txt.indexOf('download') !== -1) { t = as[i]; break; }
  }
  if (!t) {
    var btns = box.querySelectorAll('button');
    for (var j = 0; j < btns.length; j++) {
      var btxt = (btns[j].textContent || '').toLowerCase();
      if (btxt.indexOf('download') !== -1 || btns[j].querySelector('svg.lucide-download')) { t = btns[j]; break; }
    }
  }
  if (!t && as.length && !box.querySelector('[data-task-data-archive-viewer]')) t = as[0];
  if (!t) return null;
  try { t.scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
  var r = t.getBoundingClientRect();
  if (r.width < 1 || r.height < 1) return null;
  return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
})()"#;

/// Download `url` straight into `dest_dir/filename` via `curl` -- NOT
/// `ctx.download`, which attaches the current page's cookies + user-agent.
/// These zip files live on a completely different domain
/// (`*.multimodal-agentic-generation-preview.mangovibe.net`) than the task
/// page (`multimango.com`); real browsers never send cookies cross-domain,
/// but `ctx.download` does it manually, and this specific host 404s when it
/// sees a foreign cookie it doesn't recognize (confirmed: fetching the exact
/// same URL with zero cookies attached returns a clean 200). `curl` here
/// sends a bare, unauthenticated request, matching that.
pub async fn download_into(
    ctx: &WorkflowCtx,
    url: &str,
    dest_dir: &std::path::Path,
    filename: &str,
) -> Result<std::path::PathBuf> {
    std::fs::create_dir_all(dest_dir)
        .map_err(|e| GolemError::Io(format!("mkdir {}: {e}", dest_dir.display())))?;
    let dest = dest_dir.join(filename);
    let dest_str = dest
        .to_str()
        .ok_or_else(|| GolemError::Io(format!("non-UTF8 path: {}", dest.display())))?;
    let out = ctx
        .run(
            "curl",
            &["-fsSL", "-o", dest_str, url],
            None,
            Some(Duration::from_secs(120)),
        )
        .await?;
    if !out.success() {
        return Err(GolemError::Io(format!(
            "curl {url} -> {}: {}",
            dest.display(),
            out.combined().trim()
        )));
    }
    Ok(dest)
}

/// Unzip `zip_path` into `dest_dir` (creating it first) and delete the zip
/// afterward. The escape-hatch `ctx.run` shells out to the system `unzip`.
pub async fn unzip_and_cleanup(
    ctx: &WorkflowCtx,
    zip_path: &std::path::Path,
    dest_dir: &std::path::Path,
) -> Result<()> {
    std::fs::create_dir_all(dest_dir)
        .map_err(|e| GolemError::Io(format!("mkdir {}: {e}", dest_dir.display())))?;
    let out = ctx
        .run(
            "unzip",
            &[
                "-o",
                zip_path.to_str().unwrap_or_default(),
                "-d",
                dest_dir.to_str().unwrap_or_default(),
            ],
            None,
            Some(Duration::from_secs(60)),
        )
        .await?;
    if !out.success() {
        return Err(GolemError::Io(format!("unzip failed: {}", out.combined())));
    }
    std::fs::remove_file(zip_path)
        .map_err(|e| GolemError::Io(format!("delete {}: {e}", zip_path.display())))?;
    Ok(())
}

/// How a task's downloadable Task Data is exposed on the page.
pub enum TaskDataSource {
    /// An `<a href>` link ("Download all task data (ZIP)") -- fetch the href
    /// directly.
    DirectUrl(String),
    /// The newer archive-viewer layout's `<button>Download</button>` -- there
    /// is no href anywhere in the DOM (the download is wired up in JS), so
    /// the only way to get the file is to physically click the button and
    /// catch what lands in `~/Downloads`.
    DownloadButton,
}

/// The Task Data block's direct "Download all task data (ZIP)" link href:
/// prefer an anchor whose href ends in `.zip` or whose text mentions
/// "download" (some layouts put other links first), else the first `<a href>`
/// inside the wrapper div.
/// This is a same-page, top-level anchor (unlike the response zips below), so
/// it's directly readable -- no cross-origin issue here.
pub async fn task_data_zip_url(ctx: &WorkflowCtx) -> Result<Option<String>> {
    let v = ctx.eval(TASK_DATA_ZIP_URL_JS).await?;
    Ok(v.as_str().map(str::to_string).filter(|s| !s.is_empty()))
}

/// Whether the Task Data block holds an archive-viewer "Download" button
/// (see `TASK_DATA_DOWNLOAD_BUTTON_JS`).
pub async fn task_data_download_button(ctx: &WorkflowCtx) -> Result<bool> {
    let v = ctx.eval(TASK_DATA_DOWNLOAD_BUTTON_JS).await?;
    Ok(v.as_bool().unwrap_or(false))
}

/// The Task Data block's full visible text (innerText of its wrapper div).
pub async fn task_data_text(ctx: &WorkflowCtx) -> Result<String> {
    let v = ctx.eval(TASK_DATA_TEXT_JS).await?;
    Ok(v.as_str().unwrap_or_default().to_string())
}

/// Derives `<origin-of-the-named-response's-iframe>/all_files.zip` for
/// `label` ("Response A" or "Response B").
///
/// Why not click the "Copy link" button directly? It's rendered *inside* that
/// response's own `<iframe>` document, which is a different origin than the
/// multimango.com top page (confirmed: the copied URL lives on
/// `...multimodal-agentic-generation-preview.mangovibe.net`). A parent page's
/// JS can't read into a cross-origin iframe's DOM -- that's the browser's
/// same-origin policy, not a Golem limitation. But an `<iframe>` element's
/// `src` attribute is always readable from the parent (only its *contents*
/// are protected), and it's served from the exact same host as the zip. So we
/// read `iframe.src`, take its origin, and append `/all_files.zip` ourselves.
///
/// This is a pattern-based guess. If `ctx.download` on the result 404s, the
/// zip isn't at that exact path for that response and this needs adjusting.
pub async fn response_zip_url(ctx: &WorkflowCtx, label: &str) -> Result<Option<String>> {
    let js = FIND_RESPONSE_ZIP_URL_JS.replace("__LABEL__", &js_str(label));
    let v = ctx.eval(&js).await?;
    Ok(v.as_str().map(str::to_string).filter(|s| !s.is_empty()))
}

/// Poll every ~250-400ms until the page shows a way to download the Task
/// Data -- a direct link href, or the archive-viewer's Download button -- or
/// `timeout` elapses (None: this task has no downloadable data at all). The
/// page is a client-rendered SPA, so a one-shot check right after navigation
/// can race the render -- this rides that out instead of failing immediately.
/// Cancellable via `ctx.guard` (Stop button works).
pub async fn wait_for_task_data_download(
    ctx: &WorkflowCtx,
    timeout: Duration,
) -> Result<Option<TaskDataSource>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        ctx.guard().await?;
        if let Some(url) = task_data_zip_url(ctx).await? {
            return Ok(Some(TaskDataSource::DirectUrl(url)));
        }
        if task_data_download_button(ctx).await? {
            return Ok(Some(TaskDataSource::DownloadButton));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        ctx.human_pause(250, 400).await?;
    }
}

/// Same idea as [`wait_for_task_data_zip_url`], for a response's iframe.
pub async fn wait_for_response_zip_url(
    ctx: &WorkflowCtx,
    label: &str,
    timeout: Duration,
) -> Result<Option<String>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        ctx.guard().await?;
        if let Some(url) = response_zip_url(ctx, label).await? {
            return Ok(Some(url));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        ctx.human_pause(250, 400).await?;
    }
}

/// The Evaluation Criteria list, one line per criterion ("1. <text>",
/// "2. <text>", ...). Finds the "Evaluation Criteria" header span, walks up
/// to its card, then reads each row of the `div.divide-y.divide-border`
/// list beneath it -- the first child span in each row is the number, the
/// second is the criterion text.
pub async fn evaluation_criteria_text(ctx: &WorkflowCtx) -> Result<Option<String>> {
    let v = ctx.eval(EVALUATION_CRITERIA_JS).await?;
    Ok(v.as_str().map(str::to_string).filter(|s| !s.is_empty()))
}

/// Outcome of looking for the Evaluation Criteria list on the task page.
pub enum CriteriaLookup {
    /// The numbered list, one criterion per line ("1. ...", "2. ...").
    Found(String),
    /// The page is loaded (its Overall Quality card is rendered) but has no
    /// Evaluation Criteria section -- some tasks only ask for the overall pick.
    NoneOnTask,
    /// Neither the criteria nor the Overall Quality card showed up in time.
    PageNotReady,
}

/// Poll for the Evaluation Criteria list until it appears or `timeout`
/// elapses. Distinguishes "the task genuinely has no criteria" from "the
/// page hasn't loaded" by watching for the Overall Quality card, which every
/// task variant has: once that card has been visible with no criteria list
/// for several consecutive polls (grace for the SPA rendering the list a
/// beat late), the criteria are ruled absent.
pub async fn wait_for_evaluation_criteria(
    ctx: &WorkflowCtx,
    timeout: Duration,
) -> Result<CriteriaLookup> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut loaded_without_criteria = 0u32;
    loop {
        ctx.guard().await?;
        if let Some(text) = evaluation_criteria_text(ctx).await? {
            return Ok(CriteriaLookup::Found(text));
        }
        if overall_quality_present(ctx).await? {
            loaded_without_criteria += 1;
            if loaded_without_criteria >= 4 {
                return Ok(CriteriaLookup::NoneOnTask);
            }
        } else {
            loaded_without_criteria = 0;
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(if loaded_without_criteria > 0 {
                CriteriaLookup::NoneOnTask
            } else {
                CriteriaLookup::PageNotReady
            });
        }
        ctx.human_pause(250, 400).await?;
    }
}

/// Whether the page's Overall Quality card (an `<h3>Overall Quality</h3>`,
/// same marker `CLICK_OVERALL_BUTTON_JS` anchors on) is rendered.
async fn overall_quality_present(ctx: &WorkflowCtx) -> Result<bool> {
    let v = ctx
        .eval(
            r#"(function(){
  var h3s = document.querySelectorAll('h3');
  for (var i = 0; i < h3s.length; i++) {
    if ((h3s[i].textContent || '').trim() === 'Overall Quality') return true;
  }
  return false;
})()"#,
        )
        .await?;
    Ok(v.as_bool().unwrap_or(false))
}

const EVALUATION_CRITERIA_JS: &str = r#"(function(){
  var spans = document.querySelectorAll('span');
  var header = null;
  for (var i = 0; i < spans.length; i++) {
    if ((spans[i].textContent || '').trim() === 'Evaluation Criteria') { header = spans[i]; break; }
  }
  if (!header) return null;
  var card = header.closest('[class*="rounded-lg"]') || header.parentElement;
  var list = card ? card.querySelector('.divide-y') : null;
  if (!list) return null;
  var rows = list.children;
  var lines = [];
  for (var j = 0; j < rows.length; j++) {
    var row = rows[j];
    var head = row.querySelector('.flex.gap-2') || row.firstElementChild;
    var kids = head ? head.children : [];
    var num = kids[0] ? (kids[0].textContent || '').trim() : (j + 1) + '.';
    var text = kids[1] ? (kids[1].textContent || '').trim() : (head ? (head.textContent || '').trim() : '');
    if (text) lines.push(num + ' ' + text);
  }
  return lines.length ? lines.join('\n') : null;
})()"#;

// All the Task Data lookups (URL / button / click / text) find the block the
// same way. The block's class list differs between page variants -- older
// tasks use `prose prose-sm max-w-none text-sm text-foreground`, the newer
// archive-viewer layout uses `prose prose-sm dark:prose-invert max-w-none
// ...` (no `text-sm text-foreground` at all) -- so match on the common
// `prose prose-sm max-w-none` subset, preferring a candidate that contains
// the archive viewer (`[data-task-data-archive-viewer]`) or a "Task Data"
// heading, and falling back to the first match (some tasks' block has no
// heading/download link at all, only the description text).

const TASK_DATA_ZIP_URL_JS: &str = r#"(function(){
  var all = document.querySelectorAll('div.prose.prose-sm.max-w-none');
  var box = null;
  for (var b = 0; b < all.length; b++) {
    var h = all[b].querySelector('h1,h2,h3');
    if (all[b].querySelector('[data-task-data-archive-viewer]') ||
        (h && /task\s*data/i.test(h.textContent || ''))) { box = all[b]; break; }
  }
  if (!box && all.length) box = all[0];
  if (!box) return null;
  var as = box.querySelectorAll('a[href]');
  for (var i = 0; i < as.length; i++) {
    var href = as[i].getAttribute('href') || '';
    var txt = (as[i].textContent || '').toLowerCase();
    if (/\.zip(\?|$)/i.test(href) || txt.indexOf('download') !== -1) return as[i].href;
  }
  if (box.querySelector('[data-task-data-archive-viewer]')) return null;
  return as.length ? as[0].href : null;
})()"#;

// True when the block holds the newer archive-viewer's `<button>Download`
// (identified by its text or its lucide download icon) -- that layout has no
// `<a href>` to fetch, so the button must be physically clicked instead.
const TASK_DATA_DOWNLOAD_BUTTON_JS: &str = r#"(function(){
  var all = document.querySelectorAll('div.prose.prose-sm.max-w-none');
  var box = null;
  for (var b = 0; b < all.length; b++) {
    var h = all[b].querySelector('h1,h2,h3');
    if (all[b].querySelector('[data-task-data-archive-viewer]') ||
        (h && /task\s*data/i.test(h.textContent || ''))) { box = all[b]; break; }
  }
  if (!box && all.length) box = all[0];
  if (!box) return false;
  var btns = box.querySelectorAll('button');
  for (var i = 0; i < btns.length; i++) {
    var txt = (btns[i].textContent || '').toLowerCase();
    if (txt.indexOf('download') !== -1 || btns[i].querySelector('svg.lucide-download')) return true;
  }
  return false;
})()"#;

// The block's visible text, minus the archive viewer's file tree when one is
// present -- that subtree is just a listing of the files the download itself
// contains ("Input Data Files ... input_0.jpg 26.3 KB ..."), noise in the
// saved `information` file.
const TASK_DATA_TEXT_JS: &str = r#"(function(){
  var all = document.querySelectorAll('div.prose.prose-sm.max-w-none');
  var box = null;
  for (var b = 0; b < all.length; b++) {
    var h = all[b].querySelector('h1,h2,h3');
    if (all[b].querySelector('[data-task-data-archive-viewer]') ||
        (h && /task\s*data/i.test(h.textContent || ''))) { box = all[b]; break; }
  }
  if (!box && all.length) box = all[0];
  if (!box) return '';
  if (!box.querySelector('[data-task-data-archive-viewer]'))
    return box.innerText || box.textContent || '';
  var parts = [];
  var kids = box.children;
  for (var i = 0; i < kids.length; i++) {
    if (kids[i].hasAttribute('data-task-data-archive-viewer') ||
        kids[i].querySelector('[data-task-data-archive-viewer]')) continue;
    var t = kids[i].innerText || kids[i].textContent || '';
    if (t.trim()) parts.push(t);
  }
  return parts.join('\n');
})()"#;

const FIND_RESPONSE_ZIP_URL_JS: &str = r#"(function(){
  var LABEL = __LABEL__;
  function findIframe() {
    var iframes = document.querySelectorAll('iframe');
    for (var k = 0; k < iframes.length; k++) {
      if ((iframes[k].getAttribute('title') || '') === LABEL) return iframes[k];
    }
    var spans = document.querySelectorAll('span');
    for (var i = 0; i < spans.length; i++) {
      if ((spans[i].textContent || '').trim() === LABEL) {
        var card = spans[i].closest('[class*="rounded-lg"]') || spans[i].parentElement;
        if (card) {
          var inner = card.querySelector('iframe');
          if (inner) return inner;
        }
      }
    }
    return null;
  }
  var f = findIframe();
  if (!f) return null;
  var src = f.getAttribute('src') || '';
  try {
    var u = new URL(src, location.href);
    if (u.protocol !== 'http:' && u.protocol !== 'https:') return null;
    return u.origin + '/all_files.zip';
  } catch (e) {
    return null;
  }
})()"#;
