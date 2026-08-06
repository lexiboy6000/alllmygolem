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

/// Report a "Golem couldn't drive one control" problem WITHOUT blocking.
///
/// These messages all used to mean "fix it on the page and dismiss this".
/// The pipeline runs unattended now, so there is nobody to do that: the
/// message becomes a warning line and the run continues to whatever check
/// would have caught the failure anyway (the `on_new_task` test, the submit
/// button's own enabled-check, and so on).
pub async fn warn_no_block(ctx: &WorkflowCtx, msg: impl Into<String>) -> Result<()> {
    ctx.warn(format!("not waiting for a human: {}", msg.into()));
    Ok(())
}

/// Halt the chain immediately, without waiting for an acknowledgement.
///
/// The chain halts either way -- the difference is only whether Golem sits on
/// an undismissed dialog first. Unattended that turns a clean stop into a
/// wedge holding the browser and the claimed task open, so it never waits.
pub async fn halt_now(ctx: &WorkflowCtx, msg: impl Into<String>) -> GolemError {
    ctx.halt(msg)
}

/// Bring the controlled tab to the front and wait for it to actually be there.
///
/// `click_at_cursor` drives the REAL OS cursor at physical screen pixels, so a
/// press lands on whichever tab is *visible* -- not on whichever target CDP is
/// driving. Switching CDP targets returns instantly; the compositor raising and
/// repainting the window does not (on this Wayland/Hyprland setup it is well
/// over 100ms). Clicking in that gap sends the press to the previously-visible
/// page, which looks exactly like "the workflow did nothing".
pub async fn focus_and_settle(ctx: &mut WorkflowCtx) -> Result<()> {
    if let Err(e) = ctx.browser.bring_to_front().await {
        ctx.warn(format!(
            "couldn't bring the tab to the front ({e}) -- continuing, but a native click may \
             land on the wrong tab"
        ));
    }
    ctx.human_pause(900, 1500).await?;
    Ok(())
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
pub async fn ask_claude_for_answers(ctx: &WorkflowCtx, task_dir: &std::path::Path) -> Result<()> {
    let claude = if ctx.settings.claude_path.trim().is_empty() {
        "claude".to_string()
    } else {
        ctx.settings.claude_path.clone()
    };
    let model = ctx.settings.solve_model.clone();
    let effort = ctx.settings.solve_effort.clone();
    let prompt: String = ANSWER_CRITERIA_PROMPT.to_string();
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

    // Retry, because the failure this guards against is transient and the cost
    // of not retrying is enormous: an "API Error: Connection closed
    // mid-response" ends the workflow, which ends the chain, which stops the
    // whole unattended loop -- after the round already spent a ~1 GB download
    // and a full judging pass on this task.
    let answers_path = task_dir.join("claude_answers");
    // Clear any file from an earlier attempt so a stale one can't be mistaken
    // for this attempt's output.
    let _ = std::fs::remove_file(&answers_path);

    // Patient on purpose. Backoff runs 30s/60s/120s/240s, so a blip has ~7.5
    // minutes to clear -- essentially free, since the round is going to sit on
    // the 40-minute task timer afterwards regardless.
    //
    // Deliberately NOT skip-and-restart on final failure: skipping burns a real
    // task on the platform, and the thing that makes all five attempts fail is
    // a sustained outage, which would then burn task after task. Better to stop
    // the loop and let a human notice.
    const ATTEMPTS: u32 = 5;
    let mut last_err = String::new();
    for attempt in 1..=ATTEMPTS {
        ctx.guard().await?;
        match ctx.run_claude(&claude, &args, Some(task_dir), Some(timeout)).await {
            // A user Stop is a stop, never a retry.
            Err(GolemError::StoppedByUser) => return Err(GolemError::StoppedByUser),
            Err(e) => last_err = e.to_string(),
            Ok(out) => {
                // The FILE is the success signal, not the exit status. Claude
                // can drop its connection after having already written a
                // complete claude_answers (exit non-zero, output fine), and can
                // exit zero having written nothing usable.
                if read_claude_answers(&answers_path).is_ok() {
                    if attempt > 1 {
                        ctx.output(format!("claude succeeded on attempt {attempt}"));
                    }
                    return Ok(());
                }
                last_err = if out.success() {
                    "claude exited cleanly but wrote no usable claude_answers".to_string()
                } else {
                    out.combined().trim().to_string()
                };
            }
        }
        if attempt < ATTEMPTS {
            let backoff = Duration::from_secs(30u64 << (attempt - 1));
            ctx.warn(format!(
                "claude attempt {attempt}/{ATTEMPTS} failed ({last_err}) -- retrying in {}s",
                backoff.as_secs()
            ));
            let until = tokio::time::Instant::now() + backoff;
            while tokio::time::Instant::now() < until {
                ctx.guard().await?;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            let _ = std::fs::remove_file(&answers_path);
        }
    }
    Err(GolemError::Other(format!(
        "claude failed {ATTEMPTS} times running the evaluation; last error: {last_err}"
    )))
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
/// `paced` (workflow 7's first application only) spreads the whole pass over
/// a budget picked up front by [`Pacer`], so the run takes about the same
/// wall-clock time whether the rubric has five questions or thirty. Pacing is
/// PURELY timing: every answer value still comes verbatim from
/// `claude_answers`, each label keeps its own verdict through the click-order
/// shuffle below, and every click re-finds and verifies its exact target
/// (`click_until_selected`) exactly as before -- so slower never means less
/// accurate. The re-apply paths pass `false`: they run while a human reviewer
/// waits, or right before submit, and keep the old quick dwells.
pub async fn apply_answers(
    ctx: &mut WorkflowCtx,
    answers: &ClaudeAnswers,
    paced: bool,
) -> Result<(usize, Vec<String>)> {
    // The clicks drive the real OS cursor, so the task page has to actually
    // be the visible tab -- a native click lands on whatever is on screen.
    focus_and_settle(ctx).await?;
    // On the newest arena layout every rating control is inside the slide-over
    // panel, so there is nothing to click until it is open.
    ensure_criteria_panel_open(ctx).await?;
    let mut applied = 0usize;
    let mut missed: Vec<String> = Vec::new();
    // Paced runs spend a fixed budget as idle time between selections; unpaced
    // ones keep the old quick dwells, since they run with a submit waiting.
    let started = tokio::time::Instant::now();
    let mut pacer = paced.then(|| Pacer::new(answers.criteria.len()));
    if let Some(p) = &pacer {
        ctx.output(format!(
            "pacing: spreading {} answer(s) over about {} min",
            p.slots_left,
            p.budget.as_secs() / 60
        ));
    }
    for (i, a) in answers.criteria.iter().enumerate() {
        // scope the (non-Send) thread rng so it never crosses an await
        let (b_first, reread, hesitate, wander, glance) = {
            let mut rng = rand::rng();
            (
                rng.random_bool(0.5),
                rng.random_bool(0.2),
                rng.random_bool(0.15),
                rng.random_bool(0.35),
                rng.random_bool(0.2),
            )
        };
        // Paced runs get their reading time from the budget instead (spent in
        // `idle_for`, which does the same drifting and glancing), so these
        // fixed beats would be time spent twice.
        if i > 0 && pacer.is_none() {
            // a beat to "read" the next criterion before rating it
            ctx.human_pause(700, 2600).await?;
            if wander {
                // idle mouse drift while "reading" -- moves only, no clicks
                ctx.wander_cursor().await?;
            }
            if glance {
                // a quick wheel glance up/down the page -- safe, because
                // every click re-finds fresh coordinates and scrolls its own
                // target back into view before pressing
                ctx.wander_scroll().await?;
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
            match pacer.as_mut() {
                // One budget slot per selection, spent before the click.
                Some(p) => {
                    let gap = p.next_gap();
                    idle_for(ctx, gap).await?;
                }
                None => {
                    if j > 0 && hesitate {
                        // occasional second-guessing pause before the second pick
                        ctx.human_pause(1200, 3000).await?;
                    }
                }
            }
            let ok = click_criterion_button(ctx, a.number, label, want).await?;
            if ok {
                applied += 1;
                if pacer.is_none() {
                    // dwell between individual selections like a person deciding
                    ctx.human_pause(350, 1900).await?;
                }
            } else {
                missed.push(format!("#{} {} -> {}", a.number, label, want));
            }
        }
    }
    // the overall pick is the last selection, and takes the last budget slot
    match pacer.as_mut() {
        Some(p) => {
            let gap = p.next_gap();
            idle_for(ctx, gap).await?;
        }
        // weighing the overall verdict takes longer than a single row
        None => ctx.human_pause(1000, 3600).await?,
    }
    let (wander, glance) = {
        let mut rng = rand::rng();
        (rng.random_bool(0.35), rng.random_bool(0.15))
    };
    if wander {
        ctx.wander_cursor().await?;
    }
    if glance {
        ctx.wander_scroll().await?;
    }
    if click_overall_button(ctx, &answers.overall.winner).await? {
        applied += 1;
        ctx.output(format!("overall quality: {}", answers.overall.winner));
    } else {
        missed.push(format!("overall quality -> {}", answers.overall.winner));
    }
    if pacer.is_some() {
        let took = started.elapsed();
        ctx.output(format!(
            "pacing: answering took {} min {:02} s",
            took.as_secs() / 60,
            took.as_secs() % 60
        ));
    }
    Ok((applied, missed))
}

/// Hands a fixed time budget out as idle gaps between selections.
///
/// The pacing used to be open-loop: a fixed dwell after every click plus a
/// ~2-minute break every 3-7 selections. That made the run take as long as it
/// took, scaling almost linearly with the rubric -- roughly 8 minutes for six
/// questions and half an hour for thirty. A handle time that tracks question
/// count that closely is a strange thing to publish, so the total is now
/// chosen up front and spent, rather than accumulated.
struct Pacer {
    deadline: tokio::time::Instant,
    budget: Duration,
    /// Selections still to make, including the overall pick.
    slots_left: u32,
}

impl Pacer {
    fn new(criteria: usize) -> Self {
        // two selections per criterion (Response A and B), plus the overall
        let slots = (criteria.saturating_mul(2).saturating_add(1)) as u32;
        let budget = answering_budget(criteria);
        Self {
            deadline: tokio::time::Instant::now() + budget,
            budget,
            slots_left: slots.max(1),
        }
    }

    /// How long to idle before the next selection.
    ///
    /// Recomputed from what is actually left every time, so the total holds
    /// even when a click runs far longer than planned (a slow re-render, a CDP
    /// retry, a missed cursor click that had to be redone) or far shorter --
    /// overspend on one gap and the rest quietly shrink to absorb it.
    ///
    /// Individual gaps are jittered hard, and about one in six is stretched
    /// into a "stepped away" break. Both come out of the same budget, so the
    /// texture never costs extra time. The multipliers average ~1.0 so the
    /// spend stays even across the pass instead of front-loading and leaving
    /// the last few answers squeezed together.
    fn next_gap(&mut self) -> Duration {
        self.next_gap_at(tokio::time::Instant::now())
    }

    /// [`next_gap`](Self::next_gap) against an explicit clock, so the spend can
    /// be simulated in tests without sleeping through it.
    fn next_gap_at(&mut self, now: tokio::time::Instant) -> Duration {
        if self.slots_left == 0 {
            return Duration::ZERO;
        }
        let remaining = self.deadline.saturating_duration_since(now);
        let even = remaining / self.slots_left;
        let last = self.slots_left == 1;
        self.slots_left -= 1;
        let factor = if last {
            // The final gap takes exactly what is left. Jittering it would
            // shave off whatever the multiplier fell short of with no later
            // gap to make it up, so the pass would land consistently early.
            1.0
        } else {
            let mut rng = rand::rng();
            if rng.random_bool(0.16) {
                rng.random_range(1.8..2.6)
            } else {
                rng.random_range(0.40..1.15)
            }
        };
        // Never hand out more idle than the budget has left. A stretched
        // "stepped away" gap drawn with only a slot or two to go asks for more
        // than a whole even share -- 2.6x of half the remaining time is 1.3x
        // the remaining time -- and without this the pass would overrun the
        // budget it exists to enforce.
        even.mul_f64(factor).min(MAX_GAP).min(remaining)
    }
}

/// Longest single idle between two selections, so no one gap can swallow the
/// whole budget. It is deliberately generous: a two-question rubric only
/// reaches 21 minutes if the handful of gaps it has are each minutes long, and
/// that reads fine -- most of the time on a short rubric goes on reading the
/// two responses, not on the clicking. Capping tighter would make short
/// rubrics finish early no matter what the budget said.
const MAX_GAP: Duration = Duration::from_secs(480);

/// The wall-clock window a paced pass over the answers aims to fill.
///
/// Centred near 25 minutes and only very loosely tied to how many criteria
/// there are: a short rubric lands around 21 minutes and a long one around 29.
/// The curve is logarithmic, so the marginal cost of one more question is
/// tiny -- doubling the count moves the target by under two minutes -- and the
/// jitter is wide enough that neighbouring rubric sizes overlap heavily. The
/// intent is that handle time reads as a person working at their own pace,
/// with question count barely detectable in it, rather than as a function of
/// the rubric's length.
fn answering_budget(criteria: usize) -> Duration {
    let jitter: f64 = {
        let mut rng = rand::rng();
        rng.random_range(-BUDGET_JITTER_MIN..BUDGET_JITTER_MIN)
    };
    let mins = (budget_center_minutes(criteria) + jitter).clamp(BUDGET_MIN, BUDGET_MAX);
    Duration::from_secs_f64(mins * 60.0)
}

/// Minutes at or below [`NARROW_RUBRIC`] questions, and at or above
/// [`WIDE_RUBRIC`].
const BUDGET_FLOOR_MIN: f64 = 21.0;
const BUDGET_CEIL_MIN: f64 = 29.0;
/// The rubric sizes the curve is anchored between. Both ends are real sizes
/// rather than the degenerate extremes: anchoring the floor at a single
/// question would have spent most of the curve's range on rubrics that don't
/// occur, leaving a genuinely short one nearer 23 minutes than the 21 wanted.
const NARROW_RUBRIC: f64 = 3.0;
const WIDE_RUBRIC: f64 = 30.0;
/// Random slack either side of the curve. Wide enough that neighbouring
/// rubric sizes overlap, which is what keeps the question count from being
/// readable off a single handle time.
const BUDGET_JITTER_MIN: f64 = 1.5;
/// Hard bounds on the result, jitter included.
const BUDGET_MIN: f64 = 20.0;
const BUDGET_MAX: f64 = 30.0;

/// The un-jittered centre of the budget, in minutes, for a rubric of
/// `criteria` questions. Split out from [`answering_budget`] so the shape of
/// the curve can be asserted without sampling the jitter.
fn budget_center_minutes(criteria: usize) -> f64 {
    let n = (criteria.max(1)) as f64;
    let t = ((n.ln() - NARROW_RUBRIC.ln()) / (WIDE_RUBRIC.ln() - NARROW_RUBRIC.ln())).clamp(0.0, 1.0);
    BUDGET_FLOOR_MIN + (BUDGET_CEIL_MIN - BUDGET_FLOOR_MIN) * t
}

/// Spend `total` idling, the way the fixed-length break this replaces did:
/// humanized sleep chunks -- so Stop/Pause stay responsive throughout and the
/// humanize layer jitters on top -- with occasional cursor drift and page
/// glances. Timing only: it never touches which button gets clicked.
async fn idle_for(ctx: &mut WorkflowCtx, total: Duration) -> Result<()> {
    if total < Duration::from_millis(300) {
        return Ok(());
    }
    if total >= Duration::from_secs(45) {
        ctx.output(format!(
            "pacing: idling ~{}s before the next answer",
            total.as_secs()
        ));
    }
    let start = tokio::time::Instant::now();
    loop {
        let left = total.saturating_sub(start.elapsed());
        if left < Duration::from_millis(300) {
            return Ok(());
        }
        // Chunks shrink as the gap runs out, so the idle lands on its target
        // instead of overshooting it by most of a chunk.
        let hi = (left.as_millis() as u64).min(6000);
        let lo = (hi / 3).max(150).min(hi.saturating_sub(1));
        ctx.human_pause(lo, hi).await?;
        // Drifting and glancing take real time, so only when there is room.
        if left > Duration::from_secs(15) {
            // scope the (non-Send) thread rng so it never crosses an await
            let (wander, glance) = {
                let mut rng = rand::rng();
                (rng.random_bool(0.12), rng.random_bool(0.08))
            };
            if wander {
                ctx.wander_cursor().await?;
            }
            if glance {
                ctx.wander_scroll().await?;
            }
        }
    }
}

/// Re-check (without clicking) that every answer still shows as selected on
/// the page. The platform can swap the open task under us -- submitting
/// after a swap would rate a DIFFERENT task -- so the review workflow runs
/// this immediately before the multimango submit. Returns the mismatches.
pub async fn verify_answers_applied(
    ctx: &mut WorkflowCtx,
    answers: &ClaudeAnswers,
) -> Result<Vec<String>> {
    // The selections are only readable while the rating panel is open -- a
    // closed one would report every answer as missing.
    ensure_criteria_panel_open(ctx).await?;
    let mut wrong = Vec::new();
    for a in &answers.criteria {
        for (label, want) in [
            ("Response A", a.response_a.as_str()),
            ("Response B", a.response_b.as_str()),
        ] {
            let js = criteria_js(CLICK_CRITERION_BUTTON_BODY)
                .replace("__NUM__", &js_str(&format!("{}.", a.number)))
                .replace("__RESP__", &js_str(label))
                .replace("__WANT__", &js_str(want));
            let v = ctx.eval(&js).await?;
            if !v.get("selected").and_then(Value::as_bool).unwrap_or(false) {
                wrong.push(format!("#{} {} -> {}", a.number, label, want));
            }
        }
    }
    let js = criteria_js(CLICK_OVERALL_BUTTON_BODY).replace("__WANT__", &js_str(&answers.overall.winner));
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
    let js = criteria_js(CLICK_CRITERION_BUTTON_BODY)
        .replace("__NUM__", &js_str(&format!("{number}.")))
        .replace("__RESP__", &js_str(response_label))
        .replace("__WANT__", &js_str(want));
    click_until_selected(ctx, &js).await
}

/// Find and click the Overall Quality button matching `want` ("Response A",
/// "Response B", or "Tie"), in the card headed by an `<h3>Overall Quality</h3>`.
pub async fn click_overall_button(ctx: &mut WorkflowCtx, want: &str) -> Result<bool> {
    let js = criteria_js(CLICK_OVERALL_BUTTON_BODY).replace("__WANT__", &js_str(want));
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
            // The control isn't on the page at all, so there is nothing to
            // click and no coordinates to retry -- the CDP fallback below
            // can't help, since it only ever re-aims a click that missed.
            // Said plainly, because "not found" and "clicked but it didn't
            // take" have completely different causes and the summary line
            // upstream reads the same for both.
            _ => {
                if attempt == 1 {
                    ctx.output(
                        "control not found on the page -- nothing to click (this is a selector \
                         mismatch, not a missed click)",
                    );
                }
                return Ok(false);
            }
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
            ctx.click_at_cdp(x, y).await?;
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
    // Newest arena layout: submit is the "Save & Continue" button inside the
    // rating panel, so it does not exist to be found until that is open.
    ensure_criteria_panel_open(ctx).await?;
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
            ctx.click_at_cdp(x, y).await?;
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
        Ok(false) => Err(halt_now(ctx, format!(
                "{label} couldn't be retrieved ({cause}), and the Skip button couldn't be \
                 found/clicked either -- NOT restarting (that would loop on this same task). \
                 Skip the task by hand, then re-run the pipeline."
            ))
            .await),
        Err(GolemError::StoppedByUser) => Err(GolemError::StoppedByUser),
        Err(e) => Err(halt_now(ctx, format!(
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
    focus_and_settle(ctx).await?;
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
            ctx.click_at_cdp(x, y).await?;
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
- responseB/ -- Response B's submitted files (read every file, especially the HTML deliverable)\n\
- responseA/response.html and responseB/response.html -- when present, that response WAS a \
single rendered page (the app shown in the task's Response tab), and it is the whole \
deliverable: judge the response from it\n\
- responseA/model_response.txt and responseB/model_response.txt -- when present, the chat text \
that response's model wrote alongside its files; it is PART OF that response (not a task input \
or a deliverable file), so judge it as the response's accompanying message\n\n\
For EVERY criterion listed in task_data/evaluation_criteria/questions, judge Response A and \
Response B independently: does that response satisfy the criterion? Use \"Good\" if it does, \
\"Bad\" if it does not or only partially does.\n\n\
Separately, also give an OVERALL pick: which response, taken as a whole, better fulfills the \
task requirements -- \"Response A\", \"Response B\", or \"Tie\" if they're genuinely equal.\n\n\
Judging rules (these mirror the platform's official annotator guidelines):\n\
- Rate every criterion independently for each response. Never let one criterion's outcome \
bleed into another, and never assume a criterion passes because a related one did (a chart \
being present says nothing about filters, interactivity, or completeness).\n\
- Judge from the actual files: open and read EVERY file in both responses before rating \
anything. Never rate from file names, from the first file alone, or from what \
model_response.txt CLAIMS was delivered -- verify claims against the files themselves.\n\
- If a response is completely broken (no deliverable, empty output, or files that clearly \
cannot render/run), mark ALL of its criteria \"Bad\" and it must lose the overall pick.\n\
- Weigh criteria roughly equally, unless one is clearly more central to the task.\n\
- For the overall pick, working functionality and content completeness outweigh visual \
polish: a response that delivers what was asked beats a prettier one that doesn't.\n\
- Keep the overall pick consistent with your per-criterion ratings. Preferring the response \
that clearly lost on the criteria is an error -- if something important the criteria don't \
capture drives your pick, say so explicitly in the overall notes.\n\
- Use \"Tie\" sparingly: only when the responses are genuinely equal. If one is even \
slightly better, pick it.\n\
- When there are no criteria, judge the overall pick on: instruction following, visual \
quality, content completeness, and usability.\n\n\
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

const CLICK_CRITERION_BUTTON_BODY: &str = r#"
  var NUM = __NUM__;
  var RESP = __RESP__;
  var WANT = __WANT__;

  var root = findCriteriaRoot();
  if (!root) return null;
  var list = root.querySelector('.divide-y');
  if (!list) return null;

  var rows = list.children;
  for (var j = 0; j < rows.length; j++) {
    var row = rows[j];
    var head = row.querySelector('.flex.gap-2') || row.firstElementChild;
    var kids = head ? head.children : [];
    var num = kids[0] ? (kids[0].textContent || '').trim() : '';
    if (num !== NUM) continue;

    /* A response's label and its Good/Bad pair sit together, but the shape
       differs by layout: older rows are one flat group
       (<span>Response A</span><button>Good</button><button>Bad</button>),
       while the drawer nests them (<div>Response A</div> plus a separate grid
       holding the buttons). Anchoring on the label and searching its container
       covers both, where matching a fixed child count and tag name -- three
       children starting with a SPAN -- silently matched neither in the drawer. */
    var OTHER = (RESP === 'Response A') ? 'Response B' : 'Response A';
    var labels = row.querySelectorAll('span,div');
    for (var g = 0; g < labels.length; g++) {
      if ((labels[g].textContent || '').trim() !== RESP) continue;
      var scope = labels[g].parentElement;
      if (!scope) continue;
      /* Never search a container holding BOTH responses -- their buttons read
         the same, so the wrong response would get the rating. */
      if ((scope.textContent || '').indexOf(OTHER) !== -1) continue;
      var btns = scope.querySelectorAll('button');
      for (var b = 0; b < btns.length; b++) {
        var e = btns[b];
        if ((e.textContent || '').trim() !== WANT) continue;
        try { e.scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
        var r = e.getBoundingClientRect();
        if (r.width < 1 || r.height < 1) return null;
        return { x: r.left + r.width / 2, y: r.top + r.height / 2, selected: isSelected(e) };
      }
    }
    return null;
  }
  return null;
"#;

const CLICK_OVERALL_BUTTON_BODY: &str = r#"
  var WANT = __WANT__;
  var card = null;
  var heads = document.querySelectorAll('h2,h3,h4,span,div');
  for (var i = 0; i < heads.length; i++) {
    /* Headed "Overall Quality" on some task variants, plain "Overall" on
       others -- accept either. */
    if (!/^overall(\s*quality)?$/i.test((heads[i].textContent || '').trim())) continue;
    card = heads[i].closest('[class*="rounded-lg"]') || heads[i].parentElement;
    if (card) break;
  }
  /* Newest arena layout: the overall pick moved into the rating drawer, which
     may head it differently (or not at all). Fall back to the drawer itself --
     inside it, a plain <button> reading exactly "Response A"/"Response B"/"Tie"
     is the overall pick: the per-criterion buttons read Good/Bad, their
     "Response A"/"Response B" labels are <span>s, and the pane's compare tabs
     are excluded by their role. */
  if (!card) card = findCriteriaRoot();
  if (!card) return null;
  var btns = card.querySelectorAll('button');
  for (var j = 0; j < btns.length; j++) {
    if (btns[j].getAttribute('role') === 'tab') continue;
    if ((btns[j].textContent || '').replace(/\s+/g, ' ').trim() === WANT) {
      var e = btns[j];
      try { e.scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
      var r = e.getBoundingClientRect();
      if (r.width < 1 || r.height < 1) return null;
      return { x: r.left + r.width / 2, y: r.top + r.height / 2, selected: isSelected(e) };
    }
  }
  return null;
"#;

// The multimango submit control, in the two forms the site ships it.
//
// Newest arena layout: it sits INSIDE the evaluation-criteria drawer and reads
// "Save & Continue" -- there is no "Submit task" button and no Skip beside it
// to anchor on, so it is matched by its own text (`&` or "and", any spacing).
// Callers open the drawer first (`ensure_criteria_panel_open`), otherwise the
// button does not exist in the DOM to be found.
//
// Older layout: an unlabelled-by-text Submit sitting next to the page's "Skip"
// button, found by that adjacency exactly as before.
const FIND_SUBMIT_JS: &str = r#"(function(){
  function usable(b){
    if (b.disabled || b.getAttribute('aria-disabled') === 'true') return false;
    try { b.scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
    var r = b.getBoundingClientRect();
    return r.width >= 1 && r.height >= 1;
  }
  function at(b){
    var r = b.getBoundingClientRect();
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
  }
  var btns = document.querySelectorAll('button');
  for (var i = 0; i < btns.length; i++) {
    var t = (btns[i].textContent || '').replace(/\s+/g, ' ').trim();
    if (!/^save\s*(&|and)\s*continue$/i.test(t)) continue;
    if (!usable(btns[i])) continue;
    return at(btns[i]);
  }
  var skip = null;
  for (var j = 0; j < btns.length; j++) {
    if ((btns[j].textContent || '').trim().indexOf('Skip') !== -1) { skip = btns[j]; break; }
  }
  if (!skip || !skip.parentElement) return null;
  var siblings = skip.parentElement.querySelectorAll('button');
  for (var k = 0; k < siblings.length; k++) {
    var b = siblings[k];
    if (b === skip) continue;
    if (!usable(b)) continue;
    return at(b);
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
  for (var i = 0; i < ifr.length; i++) {
    /* Hosted layouts differ by src; the inline layout has no src at all, so
       fingerprint those by their srcdoc instead (length + head, to keep this
       cheap on responses that run to kilobytes). */
    var doc = ifr[i].getAttribute('srcdoc') || '';
    parts.push((ifr[i].getAttribute('src') || '') + '#' + doc.length + ':' + doc.slice(0, 120));
  }
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
    // `timeout` bounds only the wait for a download to START. Once Chrome is
    // writing a .crdownload the transfer is judged by progress instead: a big
    // ZIP on a slow link can take far longer than any fixed deadline, and
    // failing it while bytes are still arriving is just wrong.
    const STALL_LIMIT: Duration = Duration::from_secs(180);
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_partial_bytes: u64 = 0;
    let mut last_progress_at = tokio::time::Instant::now();
    let mut last_report_at = tokio::time::Instant::now();
    let mut clicks_done = 0usize;
    // First click fires immediately; each later one only after the previous
    // click has had a few seconds to produce a file.
    let mut next_click_at = tokio::time::Instant::now();
    let downloaded = loop {
        ctx.guard().await?;
        let mut candidate: Option<(std::path::PathBuf, Option<std::time::SystemTime>)> = None;
        let mut in_progress = false;
        let mut partial_bytes: u64 = 0;
        for (name, modified) in list_entries(&downloads) {
            let name_str = name.to_string_lossy();
            if name_str.ends_with(".crdownload") || name_str.ends_with(".tmp") {
                in_progress = true;
                partial_bytes += std::fs::metadata(downloads.join(name))
                    .map(|m| m.len())
                    .unwrap_or(0);
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
        let now = tokio::time::Instant::now();
        if in_progress {
            // Bytes are arriving: keep waiting as long as the partial file
            // keeps growing, and only give up once it has been flat for
            // STALL_LIMIT. The start-up `deadline` no longer applies.
            if partial_bytes > last_partial_bytes {
                last_partial_bytes = partial_bytes;
                last_progress_at = now;
            }
            if now.duration_since(last_report_at) >= Duration::from_secs(15) {
                last_report_at = now;
                ctx.output(format!(
                    "downloading... {:.1} MB so far",
                    partial_bytes as f64 / 1_048_576.0
                ));
            }
            if now.duration_since(last_progress_at) >= STALL_LIMIT {
                return Err(GolemError::Other(format!(
                    "a download started but stalled at {:.1} MB -- no progress for {}s",
                    partial_bytes as f64 / 1_048_576.0,
                    STALL_LIMIT.as_secs()
                )));
            }
        } else if now >= deadline {
            return Err(GolemError::Other(format!(
                "clicked the download control {clicks_done} time(s) but no download \
                 started in {} within {}s",
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
/// layout, see the block-finder comment above `FIND_TASK_BLOCK_FN`) -- and
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
  /* Bare-anchor fallback: skip <strong>-wrapped anchors -- that's the
     task-inputs file-browser link, which must never be clicked in this tab
     (navigating away loses the task). */
  if (!t && !box.querySelector('[data-task-data-archive-viewer]')) {
    for (var k = 0; k < as.length; k++) {
      if (!as[k].closest('strong')) { t = as[k]; break; }
    }
  }
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
    // Judge the transfer by PROGRESS, not by elapsed time. These ZIPs run to
    // ~1 GB (a real one landed at 1,016,824,007 bytes), so the old flat 120s
    // cap killed downloads that were streaming along perfectly well -- at a
    // realistic 1 MB/s that file needs ~17 minutes. --speed-time/--speed-limit
    // abort only once throughput sits under 1 KB/s for a solid minute, which
    // is a genuine stall; the outer timeout is a backstop sized so even a slow
    // link finishes a gigabyte well inside it.
    let out = ctx
        .run(
            "curl",
            &[
                "-fsSL",
                "--speed-limit",
                "1024",
                "--speed-time",
                "60",
                "-o",
                dest_str,
                url,
            ],
            None,
            Some(Duration::from_secs(7200)),
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

/// Download every file listed on a task's input file-browser page (the
/// `TaskDataSource::FileBrowserPage` layout) into `dest_dir`, preserving the
/// browser's folder structure. Returns how many files were downloaded.
///
/// A person does this by copying the link and opening it in a NEW tab (the
/// task tab must never navigate away -- that loses the claimed task). Golem
/// gets the same result without touching any tab: workflows 1-7 never switch
/// tabs by design, and the listing page is plain server-rendered HTML on the
/// same `*.mangovibe.net` host family the zips live on, which `download_into`
/// already fetches cookie-free with a bare curl. So the listing is curled to
/// a temp file (NOT stdout -- `ctx.run` streams every stdout line into the
/// log), its anchors parsed out, and each file fetched the same way. Folder
/// entries on the page are collapsed `<details>` elements with the file
/// anchors already present in the HTML, so parsing the source sees every
/// file without "unfolding" anything.
pub async fn download_file_browser_inputs(
    ctx: &WorkflowCtx,
    page_url: &str,
    dest_dir: &std::path::Path,
) -> Result<usize> {
    let html = fetch_page_html(ctx, page_url, dest_dir, ".task_inputs_index.html").await?;

    let origin = url_origin(page_url).ok_or_else(|| {
        GolemError::Other(format!("couldn't parse the file-browser URL: {page_url}"))
    })?;

    // Every anchor on the page is a file link (folders are <details>/<summary>
    // with no href; the copy buttons are onclick-only). Keep it that way by
    // filtering to same-host links anyway, and derive each file's path inside
    // task_data/ from its URL path -- that reproduces the folder tree without
    // parsing the <details> nesting.
    let files = collect_same_host_files(ctx, &origin, page_url, extract_anchor_hrefs(&html));
    if files.is_empty() {
        return Err(GolemError::Other(format!(
            "the file-browser page at {page_url} listed no downloadable files -- its layout \
             may have changed"
        )));
    }

    ctx.output(format!("the file browser lists {} file(s)", files.len()));
    download_url_list(ctx, &files, dest_dir).await?;
    Ok(files.len())
}

/// Fetch a page's HTML with the same bare-curl mechanism as [`download_into`]
/// (these hosts 404 on foreign cookies), via a temp file in `work_dir` that is
/// removed after reading -- NOT via stdout, which `ctx.run` would stream line
/// by line into the log. The temp file also never stays behind to be read by
/// the evaluation pass as if it were task/response content.
async fn fetch_page_html(
    ctx: &WorkflowCtx,
    url: &str,
    work_dir: &std::path::Path,
    tmp_name: &str,
) -> Result<String> {
    let path = download_into(ctx, url, work_dir, tmp_name).await?;
    let html = std::fs::read_to_string(&path)
        .map_err(|e| GolemError::Io(format!("read {}: {e}", path.display())))?;
    let _ = std::fs::remove_file(&path);
    Ok(html)
}

/// Resolve `hrefs` against `page_url`, keep only files on `origin`'s own
/// host, and pair each with its percent-decoded path relative to the host
/// root (`docs/Q3 report.pdf`). Deduplicates, and warns (rather than fails)
/// on individual links whose paths can't be used safely on disk.
fn collect_same_host_files(
    ctx: &WorkflowCtx,
    origin: &str,
    page_url: &str,
    hrefs: Vec<String>,
) -> Vec<(String, String)> {
    let mut seen = std::collections::HashSet::new();
    let mut files: Vec<(String, String)> = Vec::new();
    for href in hrefs {
        let Some(url) = resolve_href(origin, page_url, &href) else {
            continue;
        };
        let Some(rel) = url.strip_prefix(origin) else {
            continue;
        };
        let rel = rel.trim_start_matches('/');
        let rel = rel.split(['?', '#']).next().unwrap_or(rel);
        if rel.is_empty() {
            // the page itself
            continue;
        }
        let mut parts: Vec<String> = Vec::new();
        let mut bad = false;
        for seg in rel.split('/') {
            let seg = percent_decode(seg);
            if seg.is_empty() || seg == "." || seg == ".." || seg.contains('/') {
                bad = true;
                break;
            }
            parts.push(seg);
        }
        if bad {
            ctx.warn(format!("skipping a file link with an unusable path: {url}"));
            continue;
        }
        if seen.insert(url.clone()) {
            files.push((url, parts.join("/")));
        }
    }
    files
}

/// Download every `(url, relative-path)` pair into `dest_dir`, recreating the
/// relative paths' directory structure.
async fn download_url_list(
    ctx: &WorkflowCtx,
    files: &[(String, String)],
    dest_dir: &std::path::Path,
) -> Result<()> {
    for (url, rel) in files {
        ctx.guard().await?;
        let full = dest_dir.join(rel);
        let sub_dir = full
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| dest_dir.to_path_buf());
        let name = full
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("file")
            .to_string();
        download_into(ctx, url, &sub_dir, &name).await?;
        ctx.output(format!("downloaded {rel}"));
    }
    Ok(())
}

/// Download the named response ("Response A" / "Response B") into `dest_dir`,
/// handling both response layouts. The response's own page (the iframe `src`)
/// is peeked at first to learn which one this is:
///
/// - **Delivered-files listing** (newer): the page shows a "💬 Model
///   response" text block plus a "📎 Delivered files" list where every file
///   has an `open raw ↗` link (`<a class=raw href>`) straight to the file on
///   the same host. There is NO zip on this layout (`/all_files.zip` 404s,
///   confirmed live) and its "copy link" button doesn't work -- so each raw
///   link is downloaded directly, and the model-response text is saved as
///   `model_response.txt` alongside the files.
/// - **Anything else** (older): the deliverable itself renders in the iframe
///   and the archive lives at `<origin>/all_files.zip` -- download + unzip,
///   exactly as before. A failed peek also lands here (the zip attempt is
///   the proven path, and its failure funnels into skip-and-restart).
pub async fn download_response_into(
    ctx: &mut WorkflowCtx,
    label: &str,
    iframe_src: &str,
    dest_dir: &std::path::Path,
) -> Result<()> {
    let origin = url_origin(iframe_src).ok_or_else(|| {
        GolemError::Other(format!("{label}'s iframe src isn't a usable URL: {iframe_src}"))
    })?;

    let page_html = match fetch_page_html(ctx, iframe_src, dest_dir, ".response_page.html").await {
        Ok(html) => Some(html),
        Err(e) => {
            ctx.warn(format!(
                "couldn't peek at {label}'s page ({e}) -- assuming the zip layout"
            ));
            None
        }
    };

    let html = match page_html {
        Some(html) if response_page_is_file_listing(&html) => html,
        page => {
            let zip_url = format!("{origin}/all_files.zip");
            ctx.output(format!("{label} zip: {zip_url}"));
            match download_into(ctx, &zip_url, dest_dir, "all_files.zip").await {
                Ok(zip_path) => {
                    unzip_and_cleanup(ctx, &zip_path, dest_dir).await?;
                    ctx.output(format!("unzipped {label} into {}", dest_dir.display()));
                }
                // Newest arena layout: the response IS the page rendered in the
                // iframe -- an interactive app, with no zip and no delivered-files
                // listing behind it. A 404 here is therefore NOT the "site can't
                // serve this task's deliverables" case that `fetch`'s caller
                // skips the task over; it just means this response has nothing to
                // unpack. Save the page itself, which is what a person judges and
                // what step 7 hands to Claude.
                Err(e) if is_missing_file_error(&e) => {
                    let Some(html) = page else { return Err(e) };
                    let path = dest_dir.join("response.html");
                    std::fs::write(&path, &html)
                        .map_err(|e| GolemError::Io(format!("write {}: {e}", path.display())))?;
                    ctx.output(format!(
                        "{label} has no zip or file listing -- saved the rendered page \
                         ({} bytes) -> {}",
                        html.len(),
                        path.display()
                    ));
                }
                Err(e) => return Err(e),
            }
            return Ok(());
        }
    };

    let files = collect_same_host_files(ctx, &origin, iframe_src, extract_raw_link_hrefs(&html));
    if files.is_empty() {
        return Err(GolemError::Other(format!(
            "{label}'s page looks like a delivered-files listing but no usable file links \
             were found -- its layout may have changed"
        )));
    }
    ctx.output(format!(
        "{label} is a delivered-files listing with {} file(s) (no zip on this layout)",
        files.len()
    ));
    download_url_list(ctx, &files, dest_dir).await?;

    // The model's chat text is part of the response -- the evaluation must
    // see it too, not just the files.
    match extract_model_response_text(&html) {
        Some(text) => {
            let name = if files.iter().any(|(_, rel)| rel == "model_response.txt") {
                // a delivered file claimed the name; don't clobber it
                "model_response_page.txt"
            } else {
                "model_response.txt"
            };
            let path = dest_dir.join(name);
            std::fs::write(&path, text)
                .map_err(|e| GolemError::Io(format!("write {}: {e}", path.display())))?;
            ctx.output(format!("saved the model-response text -> {}", path.display()));
        }
        None => ctx.warn(format!(
            "{label}'s page had no readable model-response text -- saving only the files"
        )),
    }
    Ok(())
}

/// Whether a response page is the delivered-files listing rather than a
/// rendered deliverable: it must have at least one `class=raw` file link AND
/// one of the listing's own headings. A deliverable HTML that happens to use
/// a `raw` class on some anchor won't also carry those headings.
fn response_page_is_file_listing(html: &str) -> bool {
    !extract_raw_link_hrefs(html).is_empty()
        && (html.contains("Model response") || html.contains("Delivered files"))
}

/// The delivered-files listing's "💬 Model response" block, flattened to
/// plain text (tags stripped, entities decoded), or `None` when the block is
/// missing or empty.
fn extract_model_response_text(html: &str) -> Option<String> {
    // The live server emits unquoted attributes (`class=resp`); a
    // DevTools-saved copy normalizes to double quotes. Match all three forms.
    let start = [
        "<details class=\"resp\"",
        "<details class='resp'",
        "<details class=resp",
    ]
    .iter()
    .find_map(|p| html.find(p))?;
    let rest = html.get(start..)?;
    let block = rest.split("</details>").next().unwrap_or(rest);
    // drop the "💬 Model response" <summary> header itself
    let body = block
        .split_once("</summary>")
        .map(|(_, b)| b)
        .unwrap_or(block);
    let text = strip_html_to_text(body);
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| format!("{trimmed}\n"))
}

/// Flatten an HTML fragment to readable plain text: list items become
/// "- " bullets, block-level boundaries become newlines, table cells become
/// tab-separated, every other tag is dropped, entities are decoded, and runs
/// of blank lines are collapsed.
fn strip_html_to_text(fragment: &str) -> String {
    let mut out = String::with_capacity(fragment.len());
    let mut rest = fragment;
    // after a "- " bullet is opened, swallow the source's own leading
    // whitespace so items don't render as "-  item"
    let mut swallow_ws = false;
    while let Some(i) = rest.find('<') {
        let (text, after) = rest.split_at(i);
        if swallow_ws {
            let trimmed = text.trim_start();
            if !trimmed.is_empty() {
                swallow_ws = false;
                out.push_str(trimmed);
            }
        } else {
            out.push_str(text);
        }
        let Some(j) = after.find('>') else {
            rest = "";
            break;
        };
        let (tag, next) = after.split_at(j + 1);
        let t = tag.to_ascii_lowercase();
        if t.starts_with("<li") {
            out.push_str("\n- ");
            swallow_ws = true;
        } else if t.starts_with("</td") || t.starts_with("</th") {
            out.push('\t');
        } else if t.starts_with("<br")
            || t.starts_with("<p")
            || t.starts_with("</p")
            || t.starts_with("<pre")
            || t.starts_with("</pre")
            || t.starts_with("<tr")
            || t.starts_with("</tr")
            || t.starts_with("</li")
            || t.starts_with("</ul")
            || t.starts_with("</ol")
            || t.starts_with("</div")
            || t.starts_with("</table")
            || t.starts_with("</h")
        {
            out.push('\n');
        }
        rest = next;
    }
    out.push_str(rest);
    let unescaped = html_unescape(&out);
    let mut collapsed = String::with_capacity(unescaped.len());
    let mut newlines = 0u32;
    for ch in unescaped.chars() {
        if ch == '\n' {
            newlines += 1;
            if newlines > 2 {
                continue;
            }
        } else {
            newlines = 0;
        }
        collapsed.push(ch);
    }
    collapsed
}

/// Pull the href out of every `<a ...>` tag in `html`, HTML-entity-decoded.
/// Tolerant by construction: a chunk without a usable href is skipped, never
/// an error.
fn extract_anchor_hrefs(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    for chunk in html.split("<a ").skip(1) {
        let Some(tag) = chunk.split('>').next() else {
            continue;
        };
        if let Some(href) = attr_value(tag, "href")
            && !href.is_empty()
        {
            out.push(href);
        }
    }
    out
}

/// The hrefs of the delivered-files listing's `open raw ↗` links only --
/// anchors carrying the `raw` class. The model-response text can contain
/// ordinary links of its own, so plain [`extract_anchor_hrefs`] would
/// over-collect on this page.
fn extract_raw_link_hrefs(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    for chunk in html.split("<a ").skip(1) {
        let Some(tag) = chunk.split('>').next() else {
            continue;
        };
        let is_raw = attr_value(tag, "class")
            .is_some_and(|c| c.split_whitespace().any(|w| w == "raw"));
        if !is_raw {
            continue;
        }
        if let Some(href) = attr_value(tag, "href")
            && !href.is_empty()
        {
            out.push(href);
        }
    }
    out
}

/// A named attribute's value from inside one tag's text, entity-decoded.
/// Handles all three quoting forms these pages emit: the live server writes
/// unquoted (`class=raw`) and single-quoted (`href='...'`) attributes, while
/// a DevTools-saved copy normalizes everything to double quotes.
fn attr_value(tag: &str, name: &str) -> Option<String> {
    let pat = format!("{name}=");
    let mut search = tag;
    loop {
        let (before, rest) = search.split_once(&pat)?;
        // require a word boundary so e.g. `data-href=` never matches `href=`
        if before.chars().next_back().is_some_and(|c| !c.is_whitespace()) {
            search = rest;
            continue;
        }
        let value = if let Some(r) = rest.strip_prefix('"') {
            r.split('"').next().unwrap_or("")
        } else if let Some(r) = rest.strip_prefix('\'') {
            r.split('\'').next().unwrap_or("")
        } else {
            // an unquoted value runs to the next whitespace (a '>' can't
            // appear -- `tag` is already truncated at the tag's closing '>');
            // '/' must NOT terminate it, unquoted URLs are full of them
            rest.split([' ', '\t', '\n', '\r', '>']).next().unwrap_or("")
        };
        return Some(html_unescape(value));
    }
}

/// The minimal entity set attribute values on the listing page can contain.
/// `&amp;` is decoded LAST so `&amp;lt;` correctly yields a literal `&lt;`.
fn html_unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Percent-decode one URL path segment (`Invoice%20Template.pdf` ->
/// `Invoice Template.pdf`) so the file on disk gets its real name. Malformed
/// escapes pass through literally.
fn percent_decode(s: &str) -> String {
    fn hex(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while let Some(&b) = bytes.get(i) {
        if b == b'%'
            && let (Some(&hi), Some(&lo)) = (bytes.get(i + 1), bytes.get(i + 2))
            && let (Some(h), Some(l)) = (hex(hi), hex(lo))
        {
            out.push(h * 16 + l);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `https://host` (no trailing slash) from a full URL, or `None` if it
/// doesn't look like one.
fn url_origin(url: &str) -> Option<String> {
    let scheme_end = url.find("://")?;
    let rest = url.get(scheme_end + 3..)?;
    let host_len = rest.find('/').unwrap_or(rest.len());
    url.get(..scheme_end + 3 + host_len).map(str::to_string)
}

/// Resolve an anchor's href against the listing page's URL. Non-web schemes
/// (`javascript:`, `mailto:`, ...) and fragments resolve to `None`.
fn resolve_href(origin: &str, page_url: &str, href: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') {
        return None;
    }
    let lower = href.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Some(href.to_string());
    }
    if let Some(rest) = href.strip_prefix("//") {
        let scheme = origin.split("://").next().unwrap_or("https");
        return Some(format!("{scheme}://{rest}"));
    }
    if !lower.starts_with('/') && lower.split(['/', '?', '#']).next().unwrap_or("").contains(':') {
        return None;
    }
    if href.starts_with('/') {
        return Some(format!("{origin}{href}"));
    }
    let base = page_url.split(['?', '#']).next().unwrap_or(page_url);
    let dir = match base.rfind('/') {
        Some(i) if i + 1 > origin.len() => base.get(..=i)?.to_string(),
        _ => format!("{base}/"),
    };
    Some(format!("{dir}{href}"))
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
    /// The newest layout: the task prose holds a `<strong><a href>` link
    /// ("Open the task's input files (file browser)") right under its `<h1>`
    /// heading, pointing at a separate page that LISTS the input files as
    /// individual links. Following the link in the task tab would navigate
    /// the task away and lose it, so the href is read out of the DOM and the
    /// listing fetched out-of-band instead
    /// (see [`download_file_browser_inputs`]).
    FileBrowserPage(String),
}

/// The Task Data block's direct "Download all task data (ZIP)" link href:
/// prefer an anchor whose href ends in `.zip` or whose text mentions
/// "download" (some layouts put other links first), else the first `<a href>`
/// inside the wrapper div.
/// This is a same-page, top-level anchor (unlike the response zips below), so
/// it's directly readable -- no cross-origin issue here.
pub async fn task_data_zip_url(ctx: &WorkflowCtx) -> Result<Option<String>> {
    let v = ctx.eval(&task_block_js(TASK_DATA_ZIP_URL_BODY)).await?;
    Ok(v.as_str().map(str::to_string).filter(|s| !s.is_empty()))
}

/// Whether the Task Data block holds an archive-viewer "Download" button
/// (see `TASK_DATA_DOWNLOAD_BUTTON_BODY`).
pub async fn task_data_download_button(ctx: &WorkflowCtx) -> Result<bool> {
    let v = ctx.eval(&task_block_js(TASK_DATA_DOWNLOAD_BUTTON_BODY)).await?;
    Ok(v.as_bool().unwrap_or(false))
}

/// The task-inputs file-browser link's href, when the task uses that layout
/// (see `TaskDataSource::FileBrowserPage`). The link sits inside a `<strong>`
/// in the task prose block, right under its `<h1>` heading.
pub async fn task_data_file_browser_url(ctx: &WorkflowCtx) -> Result<Option<String>> {
    let v = ctx.eval(&task_block_js(TASK_DATA_FILE_BROWSER_URL_BODY)).await?;
    Ok(v.as_str().map(str::to_string).filter(|s| !s.is_empty()))
}

/// The Task Data block's full visible text (innerText of its wrapper div).
pub async fn task_data_text(ctx: &WorkflowCtx) -> Result<String> {
    let v = ctx.eval(&task_block_js(TASK_DATA_TEXT_BODY)).await?;
    Ok(v.as_str().unwrap_or_default().to_string())
}

/// The full `src` URL of the named response's iframe ("Response A" /
/// "Response B") -- the response's own page on its per-response
/// `*.multimodal-agentic-generation-preview.mangovibe.net` host. Everything a
/// response exposes (the copy-link buttons, the all_files.zip, the newer
/// delivered-files listing) lives on that host, and
/// [`download_response_into`] decides from the page itself which layout it is.
///
/// Why not click the "Copy link" / "copy link" buttons directly? They're
/// rendered *inside* that response's own `<iframe>` document, which is a
/// different origin than the multimango.com top page. A parent page's JS
/// can't read into a cross-origin iframe's DOM -- that's the browser's
/// same-origin policy, not a Golem limitation (and on the delivered-files
/// layout the copy button doesn't even work). But an `<iframe>` element's
/// `src` attribute is always readable from the parent (only its *contents*
/// are protected), so the URL is taken from there.
pub async fn response_iframe_src(ctx: &WorkflowCtx, label: &str) -> Result<Option<String>> {
    let js = FIND_RESPONSE_IFRAME_SRC_JS.replace("__LABEL__", &js_str(label));
    let v = ctx.eval(&js).await?;
    Ok(v.as_str().map(str::to_string).filter(|s| !s.is_empty()))
}

/// The named response's inlined HTML, when the task page embeds each response
/// as a `srcdoc` iframe instead of pointing at a per-response host.
///
/// On this layout there is genuinely nothing to download: no per-response
/// origin, no `all_files.zip`, no delivered-files listing. The response's whole
/// document is written into the iframe element's `srcdoc` attribute, which is
/// ordinary parent-page DOM and reads straight out. (Its *contents* still
/// can't be reached through `contentDocument` -- the iframe is sandboxed
/// WITHOUT `allow-same-origin`, so it gets an opaque origin -- but an
/// attribute on the element itself was never subject to that.)
pub async fn response_srcdoc(ctx: &WorkflowCtx, label: &str) -> Result<Option<String>> {
    let js = FIND_RESPONSE_SRCDOC_JS.replace("__LABEL__", &js_str(label));
    let v = ctx.eval(&js).await?;
    Ok(v.as_str()
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty()))
}

/// Where a response's content actually lives, which differs by layout.
pub enum ResponseSource {
    /// The response is inlined in the page as a `srcdoc` iframe -- the HTML
    /// itself, already in hand, with nothing to fetch.
    Inline(String),
    /// The response is served from its own host; the iframe's `src` is the
    /// page to start from (zip, delivered-files listing, or the page itself).
    Url(String),
}

/// Poll until the named response's content can be located, either inlined as
/// `srcdoc` or as a fetchable `src`. Same SPA-render race as the other waits,
/// so it rides out a slow first paint rather than failing instantly.
pub async fn wait_for_response_source(
    ctx: &WorkflowCtx,
    label: &str,
    timeout: Duration,
) -> Result<Option<ResponseSource>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        ctx.guard().await?;
        if let Some(html) = response_srcdoc(ctx, label).await? {
            return Ok(Some(ResponseSource::Inline(html)));
        }
        if let Some(url) = response_iframe_src(ctx, label).await? {
            return Ok(Some(ResponseSource::Url(url)));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        ctx.human_pause(250, 400).await?;
    }
}

/// Save the named response into `dir`, whichever way the page exposes it.
///
/// The inline (`srcdoc`) layout writes the response document straight to
/// `response.html` -- that IS the deliverable, and nothing is downloaded. The
/// hosted layout hands off to [`download_response_into`], which sorts out the
/// zip / delivered-files / rendered-page cases from the page itself.
pub async fn capture_response(
    ctx: &mut WorkflowCtx,
    label: &str,
    dir: &std::path::Path,
) -> Result<()> {
    std::fs::create_dir_all(dir)
        .map_err(|e| GolemError::Io(format!("mkdir {}: {e}", dir.display())))?;

    let source = wait_for_response_source(ctx, label, Duration::from_secs(15))
        .await?
        .ok_or_else(|| {
            ctx.halt(format!(
                "couldn't find {label}'s iframe after waiting 15s -- it had neither inline \
                 srcdoc content nor a usable http(s) src. Make sure you're on a loaded task \
                 page with {label} present."
            ))
        })?;

    match source {
        ResponseSource::Inline(html) => {
            let path = dir.join("response.html");
            std::fs::write(&path, &html)
                .map_err(|e| GolemError::Io(format!("write {}: {e}", path.display())))?;
            ctx.output(format!(
                "{label} is inlined in the page ({} bytes) -- nothing to download; saved -> {}",
                html.len(),
                path.display()
            ));
            Ok(())
        }
        ResponseSource::Url(src) => download_response_into(ctx, label, &src, dir).await,
    }
}

/// Bring the named response ("Response A" / "Response B") on screen on the
/// newest arena layout, where the two responses share one pane behind a tab
/// strip (`[role=tab]` "Response A" / "Response B" over
/// `#rater-compare-panel-left` / `-right`) and only the selected one is
/// visible. A person clicks the tab to see that response, so Golem does too.
///
/// Older layouts render both responses side by side with no tab strip at all;
/// there is then nothing to click and this is a no-op. A tab that is present
/// but won't select is a warning rather than an error: the iframe's `src` is
/// readable from the parent whether or not its panel is the visible one, so
/// the download can still go ahead.
pub async fn activate_response_tab(ctx: &mut WorkflowCtx, label: &str) -> Result<()> {
    let js = FIND_RESPONSE_TAB_JS.replace("__LABEL__", &js_str(label));
    if ctx.eval(&js).await?.is_null() {
        return Ok(());
    }
    if click_until_selected(ctx, &js).await? {
        ctx.output(format!("opened the {label} tab"));
        // The panel swap is a visibility flip plus the iframe's own paint --
        // give it a beat before anything reads the pane.
        ctx.human_pause(600, 1400).await?;
    } else {
        ctx.warn(format!(
            "couldn't select the {label} tab -- reading its panel anyway (both the srcdoc and \
             the src are readable while the panel is hidden)"
        ));
    }
    Ok(())
}

/// Poll every ~250-400ms until the page shows a way to download the Task
/// Data -- a direct link href, the archive-viewer's Download button, or the
/// task-inputs file-browser link -- or `timeout` elapses (None: this task has
/// no downloadable data at all). The page is a client-rendered SPA, so a
/// one-shot check right after navigation can race the render -- this rides
/// that out instead of failing immediately. Cancellable via `ctx.guard`
/// (Stop button works).
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
        if let Some(url) = task_data_file_browser_url(ctx).await? {
            return Ok(Some(TaskDataSource::FileBrowserPage(url)));
        }
        // A rendered "User request" brief with no link and no button in it has
        // nothing to download and never will -- don't sit out the full timeout
        // on every task just to conclude that.
        if task_data_has_no_download(ctx).await? {
            return Ok(None);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        ctx.human_pause(250, 400).await?;
    }
}

/// Whether the page is the inline-brief layout, already rendered, with no
/// download control of any kind -- i.e. this task has no downloadable data.
/// Requires non-empty text so a not-yet-rendered block isn't mistaken for one.
pub async fn task_data_has_no_download(ctx: &WorkflowCtx) -> Result<bool> {
    let v = ctx.eval(&task_block_js(TASK_DATA_NO_DOWNLOAD_BODY)).await?;
    Ok(v.as_bool().unwrap_or(false))
}

/// The Evaluation Criteria list, one line per criterion ("1. <text>",
/// "2. <text>", ...). Finds the "Evaluation Criteria" header span, walks up
/// to its card, then reads each row of the `div.divide-y.divide-border`
/// list beneath it -- the first child span in each row is the number, the
/// second is the criterion text.
pub async fn evaluation_criteria_text(ctx: &WorkflowCtx) -> Result<Option<String>> {
    let v = ctx.eval(&criteria_js(EVALUATION_CRITERIA_BODY)).await?;
    Ok(v.as_str().map(str::to_string).filter(|s| !s.is_empty()))
}

/// Whether the rating UI is on screen: either the newest layout's slide-over
/// drawer is open, or an older layout renders the criteria inline (in which
/// case there is no drawer and nothing to open).
pub async fn criteria_panel_open(ctx: &WorkflowCtx) -> Result<bool> {
    let v = ctx.eval(&criteria_js(CRITERIA_ROOT_PRESENT_BODY)).await?;
    Ok(v.as_bool().unwrap_or(false))
}

/// Open the evaluation-criteria drawer if the page has one and it is shut.
///
/// On the newest arena layout the whole rating UI -- the per-criterion
/// Good/Bad buttons, the overall pick and the "Save & Continue" submit -- lives
/// in a slide-over panel that starts collapsed to a 48px rail on the right,
/// labelled "Evaluation criteria" under an icon. Nothing inside it exists in
/// the DOM until the rail's icon is clicked, so every step that reads or
/// clicks the rating UI has to open it first (steps 6, 7 and 8 all do).
///
/// Returns whether the rating UI is on screen afterwards. Older layouts render
/// the criteria inline and report `true` immediately with nothing clicked.
pub async fn ensure_criteria_panel_open(ctx: &mut WorkflowCtx) -> Result<bool> {
    if criteria_panel_open(ctx).await? {
        return Ok(true);
    }
    // No toggle at all: either an older inline layout that just hasn't
    // rendered yet, or a page that isn't the task page. Either way there is
    // nothing here to click.
    if ctx.eval(FIND_CRITERIA_TOGGLE_JS).await?.is_null() {
        return Ok(false);
    }
    // The clicks below drive the real cursor, so the tab has to be in front.
    focus_and_settle(ctx).await?;
    const ATTEMPTS: usize = 3;
    for attempt in 1..=ATTEMPTS {
        let v = ctx.eval(FIND_CRITERIA_TOGGLE_JS).await?;
        let (Some(x), Some(y)) = (
            v.get("x").and_then(Value::as_f64),
            v.get("y").and_then(Value::as_f64),
        ) else {
            break;
        };
        let (x, y) = jittered(ctx, x, y);
        if attempt < ATTEMPTS {
            ctx.click_at_cursor(x, y).await?;
        } else {
            ctx.click_at_cdp(x, y).await?;
        }
        // The drawer slides in over ~200ms; give the transition room plus a
        // beat for React to mount the rows inside it.
        ctx.human_pause(600, 1200).await?;
        if criteria_panel_open(ctx).await? {
            ctx.output("opened the evaluation criteria panel");
            return Ok(true);
        }
    }
    Ok(false)
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

/// Whether the rating UI is rendered enough to say the page has loaded --
/// used to tell "this task genuinely has no criteria" apart from "the page
/// isn't ready yet". Older layouts show an Overall Quality card
/// (`CLICK_OVERALL_BUTTON_BODY`'s marker); the newest one shows the rating
/// drawer instead, which may head its overall pick differently, so an open
/// drawer counts as loaded too.
async fn overall_quality_present(ctx: &WorkflowCtx) -> Result<bool> {
    let v = ctx
        .eval(&criteria_js(
            r#"
  var heads = document.querySelectorAll('h2,h3,h4,span,div');
  for (var i = 0; i < heads.length; i++) {
    if (/^overall(\s*quality)?$/i.test((heads[i].textContent || '').trim())) return true;
  }
  return !!findCriteriaRoot();
"#,
        ))
        .await?;
    Ok(v.as_bool().unwrap_or(false))
}

// Everything that reads or clicks the rating UI resolves its container the
// same way, through the shared `findCriteriaRoot()` below.
//
// Two layouts are in play. Older tasks render an inline "Evaluation Criteria"
// card in the page body. The newest arena layout moves the whole thing into a
// slide-over drawer headed "Rate this comparison", which is collapsed to a
// right-hand rail until its icon is clicked -- so the drawer is checked first,
// and only a drawer that is actually on screen counts (the collapsed one keeps
// `aria-hidden="true"` and sits translated off the viewport).
//
// The rail's own label reads "Evaluation criteria" (lower-case c) while the
// old inline header reads "Evaluation Criteria", hence the case-insensitive
// match -- and requiring a `.divide-y` list under that header keeps the rail's
// label from being mistaken for the criteria card itself.
const FIND_CRITERIA_ROOT_FN: &str = r#"
/* Whether a rating button is the chosen one. Two idioms are in play and both
   have to be honoured, because reading selection wrongly is not a cosmetic
   bug: click_until_selected re-clicks a button it believes didn't take, so a
   false negative presses an already-correct choice again (which can toggle it
   back off) and then reports a perfectly good rating as missed.
     - class-driven (criterion Good/Bad, and the drawer's overall pick): the
       neutral `bg-background` is swapped for a filled colour when chosen.
     - style-driven (the older inline overall card): classes never change and
       the pick shows up as an inline background-color. */
function isSelected(e){
  if ((e.getAttribute('style') || '').indexOf('background-color') !== -1) return true;
  return (e.className || '').toString().indexOf('bg-background') === -1;
}
function onScreen(e){
  if (!e || e.getAttribute('aria-hidden') === 'true') return false;
  var r = e.getBoundingClientRect();
  if (r.width < 40 || r.height < 40) return false;
  return r.right > 0 && r.left < (window.innerWidth || 0);
}
function findCriteriaRoot(){
  var panels = document.querySelectorAll('aside,[role="dialog"],[role="complementary"]');
  for (var i = 0; i < panels.length; i++) {
    if (!onScreen(panels[i])) continue;
    if (/rate this comparison/i.test(panels[i].textContent || '')) return panels[i];
  }
  var els = document.querySelectorAll('span,div,h2,h3');
  for (var j = 0; j < els.length; j++) {
    if (!/^evaluation\s*criteria$/i.test((els[j].textContent || '').trim())) continue;
    var card = els[j].closest('[class*="rounded-lg"]') || els[j].parentElement;
    if (card && card.querySelector('.divide-y')) return card;
  }
  return null;
}
"#;

/// Wrap one of the criteria bodies below (each of which calls
/// `findCriteriaRoot()`) into an evaluatable IIFE.
fn criteria_js(body: &str) -> String {
    format!("(function(){{{FIND_CRITERIA_ROOT_FN}\n{body}\n}})()")
}

const CRITERIA_ROOT_PRESENT_BODY: &str = "return !!findCriteriaRoot();";

// The rail (or, on a narrow window, the floating "Rate · n/m" pill) that opens
// the drawer. Both carry the same `aria-label`, and only one of the two is
// ever visible, so the visible one wins.
const FIND_CRITERIA_TOGGLE_JS: &str = r#"(function(){
  var btns = document.querySelectorAll('button[aria-label]');
  for (var i = 0; i < btns.length; i++) {
    var b = btns[i];
    if (!/^open evaluation criteria/i.test(b.getAttribute('aria-label') || '')) continue;
    if (b.disabled || b.getAttribute('aria-disabled') === 'true') continue;
    try { b.scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
    var r = b.getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
  }
  return null;
})()"#;

const EVALUATION_CRITERIA_BODY: &str = r#"
  var root = findCriteriaRoot();
  if (!root) return null;
  var list = root.querySelector('.divide-y');
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
"#;

// All the Task Data lookups (URL / button / text) find the block through the
// shared `findTaskBlock()` below, which reports both the block and whether it
// is the newest layout's request brief.
//
// Three layouts are in play:
//   * older tasks: a `prose prose-sm max-w-none text-sm text-foreground` block
//     headed "Task Data";
//   * the archive-viewer layout: the same block with
//     `[data-task-data-archive-viewer]` inside and no `text-sm text-foreground`
//     at all -- hence matching on the common `prose prose-sm max-w-none`
//     subset, preferring a candidate holding the viewer or a "Task Data"
//     heading and falling back to the first match;
//   * the newest arena layout: a left-rail card headed "User request" whose
//     body prose IS the task description ("Task data is the sentence under
//     User request"). It has no "Task Data" heading and no download control,
//     so it reports `brief: true` and the bare-anchor ZIP fallback is
//     suppressed for it -- a link inside that prose is part of the request
//     being described, not an archive to fetch.
const FIND_TASK_BLOCK_FN: &str = r#"
function findTaskBlock(){
  var els = document.querySelectorAll('div,span,h1,h2,h3,h4');
  for (var i = 0; i < els.length; i++) {
    if (!/^user\s*request$/i.test((els[i].textContent || '').trim())) continue;
    var card = els[i].closest('section') || els[i].parentElement;
    var body = card ? card.querySelector('.prose') : null;
    if (!body) body = els[i].nextElementSibling;
    if (body) return { box: body, brief: true };
  }
  var all = document.querySelectorAll('div.prose.prose-sm.max-w-none');
  var box = null;
  for (var b = 0; b < all.length; b++) {
    var h = all[b].querySelector('h1,h2,h3');
    if (all[b].querySelector('[data-task-data-archive-viewer]') ||
        (h && /task\s*data/i.test(h.textContent || ''))) { box = all[b]; break; }
  }
  if (!box && all.length) box = all[0];
  return box ? { box: box, brief: false } : null;
}
"#;

/// Wrap one of the Task Data lookup bodies below (each of which calls
/// `findTaskBlock()`) into an evaluatable IIFE.
fn task_block_js(body: &str) -> String {
    format!("(function(){{{FIND_TASK_BLOCK_FN}\n{body}\n}})()")
}

const TASK_DATA_ZIP_URL_BODY: &str = r#"
  var found = findTaskBlock();
  if (!found) return null;
  var box = found.box;
  var as = box.querySelectorAll('a[href]');
  for (var i = 0; i < as.length; i++) {
    var href = as[i].getAttribute('href') || '';
    var txt = (as[i].textContent || '').toLowerCase();
    if (/\.zip(\?|$)/i.test(href) || txt.indexOf('download') !== -1) return as[i].href;
  }
  if (box.querySelector('[data-task-data-archive-viewer]')) return null;
  /* The request brief never carries a task-data archive, so an anchor in it
     is part of the request text -- never guess that one is a download. */
  if (found.brief) return null;
  /* Bare-anchor fallback: skip anchors wrapped in <strong> -- that's the
     task-inputs file-browser link (a page LISTING the files, not a zip),
     detected separately by TASK_DATA_FILE_BROWSER_URL_BODY. */
  for (var k = 0; k < as.length; k++) {
    if (!as[k].closest('strong')) return as[k].href;
  }
  return null;
"#;

// True when the block holds the newer archive-viewer's `<button>Download`
// (identified by its text or its lucide download icon) -- that layout has no
// `<a href>` to fetch, so the button must be physically clicked instead.
const TASK_DATA_DOWNLOAD_BUTTON_BODY: &str = r#"
  var found = findTaskBlock();
  if (!found) return false;
  var btns = found.box.querySelectorAll('button');
  for (var i = 0; i < btns.length; i++) {
    var txt = (btns[i].textContent || '').toLowerCase();
    if (txt.indexOf('download') !== -1 || btns[i].querySelector('svg.lucide-download')) return true;
  }
  return false;
"#;

// The task-inputs file-browser link: an `<a href>` wrapped in a `<strong>`
// inside the task block ("Open the task's input files (file browser)").
// Prefers a link whose text actually says file browser / input files (in case
// the description prose ever bolds an ordinary link too), falling back to the
// first strong-wrapped anchor. Returns the ABSOLUTE href -- the link is never
// clicked (navigating this tab away loses the task). This one is left live for
// the request brief too: if a task ever states its inputs that way, the link
// is still the right thing to follow out-of-band.
const TASK_DATA_FILE_BROWSER_URL_BODY: &str = r#"
  var found = findTaskBlock();
  if (!found) return null;
  var links = found.box.querySelectorAll('strong a[href]');
  var pick = null;
  for (var i = 0; i < links.length; i++) {
    var txt = links[i].textContent || '';
    if (/file\s*browser|input\s*files?|task\s*inputs?/i.test(txt)) { pick = links[i]; break; }
    if (!pick) pick = links[i];
  }
  if (!pick) return null;
  try {
    var u = new URL(pick.getAttribute('href') || '', location.href);
    if (u.protocol !== 'http:' && u.protocol !== 'https:') return null;
    return u.href;
  } catch (e) { return null; }
"#;

// True only for a rendered request brief holding no anchor and no button --
// the layout where the task is stated as prose and there is nothing to fetch.
// Anything else (any control at all, or an empty/unrendered block) returns
// false so the caller keeps polling.
const TASK_DATA_NO_DOWNLOAD_BODY: &str = r#"
  var found = findTaskBlock();
  if (!found || !found.brief) return false;
  var box = found.box;
  if (!(box.innerText || box.textContent || '').trim()) return false;
  if (box.querySelector('a[href]')) return false;
  if (box.querySelector('button')) return false;
  return true;
"#;

// The block's visible text, minus the archive viewer's file tree when one is
// present -- that subtree is just a listing of the files the download itself
// contains ("Input Data Files ... input_0.jpg 26.3 KB ..."), noise in the
// saved `information` file.
const TASK_DATA_TEXT_BODY: &str = r#"
  var found = findTaskBlock();
  if (!found) return '';
  var box = found.box;
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
"#;

// A response inlined into the page as a `srcdoc` iframe. Read as an attribute
// off the parent's own DOM, so the iframe's opaque sandbox origin is no
// obstacle -- see `response_srcdoc`.
const FIND_RESPONSE_SRCDOC_JS: &str = r#"(function(){
  var LABEL = __LABEL__;
  var iframes = document.querySelectorAll('iframe');
  for (var i = 0; i < iframes.length; i++) {
    if ((iframes[i].getAttribute('title') || '') !== LABEL) continue;
    var doc = iframes[i].getAttribute('srcdoc');
    if (doc && doc.trim()) return doc;
  }
  return null;
})()"#;

// The tab that reveals a response on the newest arena layout. `selected` reads
// the tab's own `aria-selected`, so `click_until_selected` verifies the switch
// the same way it verifies a rating.
const FIND_RESPONSE_TAB_JS: &str = r#"(function(){
  var LABEL = __LABEL__;
  var tabs = document.querySelectorAll('[role="tab"]');
  for (var i = 0; i < tabs.length; i++) {
    var t = tabs[i];
    if ((t.textContent || '').replace(/\s+/g, ' ').trim() !== LABEL) continue;
    var sel = t.getAttribute('aria-selected') === 'true';
    try { t.scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
    var r = t.getBoundingClientRect();
    if (r.width < 1 || r.height < 1) return null;
    return { x: r.left + r.width / 2, y: r.top + r.height / 2, selected: sel };
  }
  return null;
})()"#;

const FIND_RESPONSE_IFRAME_SRC_JS: &str = r#"(function(){
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
    return u.href;
  } catch (e) {
    return null;
  }
})()"#;

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGIN: &str =
        "https://13bd4fc8-ef7c-5ee7-aec1-12e7551eb868.multimodal-agentic-generation-preview.mangovibe.net";

    /// Anchor markup copied verbatim from a real file-browser listing page --
    /// the live server single-quotes attributes (`href='...'`) while a
    /// DevTools-saved copy normalizes them to double quotes, so both forms
    /// are exercised -- plus a nested-folder link, a relative link, and a
    /// copy <button> (no href) like the real page renders next to every file.
    #[test]
    fn listing_hrefs_extract_and_resolve() {
        let page_url = format!("{ORIGIN}/");
        let html = format!(
            r#"<div class="f"><a href='{ORIGIN}/Invoice_Template.pdf' download target='_blank' rel='noopener'>Invoice_Template.pdf</a><button class="cp" onclick="cp('x',this)">📋</button></div>
<details><summary>docs</summary><div class="ind"><div class="f"><a href="/docs/Q3%20report&amp;final.pdf" download="">Q3 report</a></div></div></details>
<div class="f"><a href="notes.txt">notes.txt</a></div>
<a href="javascript:void(0)">nope</a>"#
        );
        let origin = url_origin(&page_url).unwrap_or_default();
        assert_eq!(origin, ORIGIN);
        let resolved: Vec<String> = extract_anchor_hrefs(&html)
            .iter()
            .filter_map(|h| resolve_href(&origin, &page_url, h))
            .collect();
        assert_eq!(
            resolved,
            vec![
                format!("{ORIGIN}/Invoice_Template.pdf"),
                format!("{ORIGIN}/docs/Q3%20report&final.pdf"),
                format!("{ORIGIN}/notes.txt"),
            ]
        );
    }

    /// The delivered-files response layout, with the raw-link markup copied
    /// verbatim from the LIVE server (unquoted `class=raw`, single-quoted
    /// href) plus a DevTools-normalized double-quoted variant, and an
    /// ordinary link inside the model-response text that must NOT be
    /// collected as a delivered file.
    #[test]
    fn delivered_files_listing_parses() {
        let host = "https://bfdb75b0-362a-5edc-81da-dc7d481b999a.multimodal-agentic-generation-preview.mangovibe.net";
        let html = format!(
            r#"<body><div class="tip">Click a header</div><details class=resp open><summary>💬 Model response</summary><div class="frag"><p>Files generated, see <a href="https://example.com/doc">docs</a>:</p><ul><li> <code>Invoice_Paid.pdf</code> — updated invoice</li></ul></div></details><div class="arts"><h2>📎 Delivered files (2)</h2><details><summary>📄 Invoice_Paid.pdf<a class=raw href='{host}/Invoice_Paid.pdf' target=_blank rel=noopener onclick='event.stopPropagation()'>open raw ↗</a><button class="cp">copy link</button></summary></details><details><summary>📄 Invoice_Template2.pdf<a class="raw" href="{host}/Invoice_Template2.pdf" target="_blank" rel="noopener">open raw ↗</a></summary></details></div></body>"#
        );
        assert!(response_page_is_file_listing(&html));
        assert_eq!(
            extract_raw_link_hrefs(&html),
            vec![
                format!("{host}/Invoice_Paid.pdf"),
                format!("{host}/Invoice_Template2.pdf"),
            ]
        );
        let text = extract_model_response_text(&html).unwrap_or_default();
        assert!(text.contains("Files generated"));
        assert!(text.contains("- Invoice_Paid.pdf — updated invoice"));
        assert!(!text.contains('<'), "tags must be stripped: {text:?}");
        assert!(!text.contains("Model response"), "summary header must be dropped");

        // an ordinary deliverable HTML page is NOT mistaken for the listing
        let deliverable = r#"<html><body><h1>Invoice</h1><a href="app.js">app</a></body></html>"#;
        assert!(!response_page_is_file_listing(deliverable));
    }

    #[test]
    fn attr_value_quoting_forms() {
        assert_eq!(
            attr_value("class=raw href='https://h/a b.pdf' rel=noopener", "href"),
            Some("https://h/a b.pdf".to_string())
        );
        assert_eq!(
            attr_value(r#"class="raw" href="https://h/x?a=1&amp;b=2""#, "href"),
            Some("https://h/x?a=1&b=2".to_string())
        );
        assert_eq!(
            attr_value("href=https://h/plain.pdf target=_blank", "href"),
            Some("https://h/plain.pdf".to_string())
        );
        // word boundary: data-href= must not satisfy href=
        assert_eq!(attr_value("data-href=nope", "href"), None);
        assert_eq!(
            attr_value("data-href=nope href=yes", "href"),
            Some("yes".to_string())
        );
    }

    #[test]
    fn percent_decoding_and_bad_segments() {
        assert_eq!(percent_decode("Q3%20report.pdf"), "Q3 report.pdf");
        assert_eq!(percent_decode("plain.txt"), "plain.txt");
        // malformed escapes pass through untouched
        assert_eq!(percent_decode("50%25 off %zz"), "50% off %zz");
        // an encoded traversal decodes to ".." so the caller's check catches it
        assert_eq!(percent_decode("%2E%2E"), "..");
        // an encoded slash surfaces for the caller's contains('/') check
        assert_eq!(percent_decode("a%2Fb"), "a/b");
    }
}

#[cfg(test)]
mod pacing_tests {
    use super::*;

    /// The anchors asked for: a short rubric lands near 21 minutes, a long one
    /// near 29, and the middle sits around 25.
    #[test]
    fn the_budget_curve_hits_its_anchors() {
        // at or below the narrow anchor, and at or above the wide one, the
        // curve is flat -- rubrics that extreme are all treated the same
        assert!((budget_center_minutes(3) - 21.0).abs() < 0.01);
        assert!((budget_center_minutes(1) - 21.0).abs() < 0.01);
        assert!((budget_center_minutes(30) - 29.0).abs() < 0.01);
        assert!((budget_center_minutes(90) - 29.0).abs() < 0.01);
        // a mid-sized rubric is near the 25 min centre
        let mid = budget_center_minutes(10);
        assert!((24.0..=26.0).contains(&mid), "10 questions -> {mid} min");
    }

    /// The whole point: question count must barely show through. One more
    /// question is worth seconds, and even a doubling is worth under two
    /// minutes -- comfortably inside the +/-1.5 min of jitter laid on top.
    #[test]
    fn the_correlation_with_question_count_is_subtle() {
        // The steepest step is at the narrow end, where one more question is
        // a large proportional change -- 3 to 4 is a third more work. Even
        // there it is worth about a minute, well inside the jitter.
        for n in 3..40usize {
            let step = budget_center_minutes(n + 1) - budget_center_minutes(n);
            assert!(
                (0.0..1.1).contains(&step),
                "question {n} -> {} added {step} min, too coarse a step",
                n + 1
            );
        }
        // and past the first few it is seconds, not minutes
        for n in 8..40usize {
            let step = budget_center_minutes(n + 1) - budget_center_minutes(n);
            assert!(step < 0.5, "question {n} -> {} added {step} min", n + 1);
        }
        for n in [3usize, 5, 8, 10, 15] {
            let doubling = budget_center_minutes(n * 2) - budget_center_minutes(n);
            assert!(
                doubling < 2.0 * BUDGET_JITTER_MIN,
                "doubling {n} -> {} moved the budget {doubling} min, more than the jitter hides",
                n * 2
            );
        }
    }

    /// More questions never means less time, however subtle the slope.
    #[test]
    fn the_budget_never_decreases_with_more_questions() {
        for n in 1..80usize {
            assert!(
                budget_center_minutes(n + 1) >= budget_center_minutes(n) - f64::EPSILON,
                "budget went down between {n} and {}",
                n + 1
            );
        }
    }

    /// Jitter included, and for any rubric size, the budget stays inside the
    /// 25 +/- 5 minute window.
    #[test]
    fn the_budget_stays_within_25_plus_or_minus_5() {
        for criteria in [0usize, 1, 3, 6, 12, 25, 40, 100] {
            for _ in 0..500 {
                let mins = answering_budget(criteria).as_secs_f64() / 60.0;
                assert!(
                    (20.0..=30.0).contains(&mins),
                    "{criteria} questions produced a {mins} min budget"
                );
            }
        }
    }

    /// A rubric with no criteria at all still gets a real budget rather than
    /// dividing by zero -- only the overall pick is clicked in that case.
    #[test]
    fn an_empty_rubric_still_paces() {
        let p = Pacer::new(0);
        assert_eq!(p.slots_left, 1);
        assert!(p.budget >= Duration::from_secs(20 * 60));
    }

    /// Walk a pacer through a whole rubric against a simulated clock, with
    /// each click costing `click` on top of the gap. Returns how long the
    /// pass took end to end, and its budget.
    fn simulate(criteria: usize, click: Duration) -> (Duration, Duration) {
        let mut p = Pacer::new(criteria);
        let budget = p.budget;
        let start = p.deadline - budget;
        let mut now = start;
        for _ in 0..p.slots_left {
            now += p.next_gap_at(now) + click;
        }
        assert_eq!(p.slots_left, 0, "every slot should have been handed out");
        (now - start, budget)
    }

    /// The point of the whole exercise: however many questions there are, the
    /// pass lands near its budget rather than running as long as it happens to
    /// take. The clock has to be simulated -- the gaps self-correct against
    /// elapsed time, so a pacer polled without time passing is meaningless.
    #[test]
    fn a_paced_pass_lands_near_its_budget() {
        for criteria in [3usize, 5, 8, 12, 20, 30] {
            for _ in 0..200 {
                let (took, budget) = simulate(criteria, Duration::from_millis(800));
                let ratio = took.as_secs_f64() / budget.as_secs_f64();
                assert!(
                    (0.95..=1.15).contains(&ratio),
                    "{criteria} questions took {took:?} against a {budget:?} budget"
                );
            }
        }
    }

    /// What the pacing is actually for: the wall time barely moves as the
    /// rubric grows. One question and thirty are a 30x difference in work and
    /// under nine minutes apart, where the old open-loop pacing put them
    /// roughly 8 and 30 minutes apart.
    #[test]
    fn wall_time_barely_tracks_the_question_count() {
        let mean = |criteria: usize| {
            let n = 300;
            (0..n)
                .map(|_| simulate(criteria, Duration::from_millis(800)).0.as_secs_f64() / 60.0)
                .sum::<f64>()
                / n as f64
        };
        // the anchors asked for: a short rubric near 21, a long one near 29
        let short = mean(3);
        let mid = mean(10);
        let long = mean(30);
        assert!((20.0..=23.0).contains(&short), "3 questions -> {short} min");
        assert!((24.0..=27.0).contains(&mid), "10 questions -> {mid} min");
        assert!((27.0..=30.0).contains(&long), "30 questions -> {long} min");
        assert!(
            long - short < 9.0,
            "3 vs 30 questions differ by {} min -- too readable",
            long - short
        );
    }

    /// One or two questions can't fill the budget: there are only three or
    /// five gaps to spread it over and no single one may run forever. They
    /// finish early rather than staring, which is the right trade -- but they
    /// must still take the better part of the window, not minutes.
    #[test]
    fn a_degenerate_rubric_finishes_early_but_not_fast() {
        for criteria in [1usize, 2] {
            let slots = (criteria * 2 + 1) as u32;
            let click = Duration::from_millis(800);
            for _ in 0..200 {
                let (took, budget) = simulate(criteria, click);
                // the only thing it can exceed the budget by is the clicking
                assert!(
                    took <= budget + click * slots,
                    "{criteria} questions overran: {took:?} against {budget:?}"
                );
                assert!(
                    took >= Duration::from_secs(14 * 60),
                    "{criteria} questions rushed it in {took:?}"
                );
            }
        }
    }

    /// Slow clicks come out of the gaps rather than being added on top, so a
    /// page that fights back doesn't stretch the handle time.
    #[test]
    fn slow_clicks_are_absorbed_rather_than_added() {
        for _ in 0..200 {
            // 8s per selection on a 12-question rubric is ~3.5 min of clicking
            let (took, budget) = simulate(12, Duration::from_secs(8));
            assert!(
                took.as_secs_f64() <= budget.as_secs_f64() * 1.15,
                "slow clicks pushed the pass to {took:?} against a {budget:?} budget"
            );
        }
    }

    /// Clicking so slow it eats the whole budget can only ever overrun by the
    /// clicking itself: once the deadline is past, `next_gap` hands out zero
    /// rather than going negative or wrapping.
    #[test]
    fn a_budget_eaten_by_clicking_just_stops_idling() {
        const CRITERIA: usize = 12;
        let slots = (CRITERIA * 2 + 1) as u32;
        let click = Duration::from_secs(120);
        for _ in 0..50 {
            let (took, budget) = simulate(CRITERIA, click);
            assert!(
                took <= click * slots + budget,
                "runaway overrun: {took:?} for {slots} slots of {click:?} plus a {budget:?} budget"
            );
        }
    }

    /// No single gap may swallow the budget, however few slots there are to
    /// spread it over.
    #[test]
    fn no_single_gap_exceeds_the_cap() {
        for criteria in [0usize, 1, 2, 40] {
            let mut p = Pacer::new(criteria);
            let start = p.deadline - p.budget;
            let mut now = start;
            for _ in 0..p.slots_left {
                let g = p.next_gap_at(now);
                assert!(g <= MAX_GAP, "{criteria} questions produced a {g:?} gap");
                now += g;
            }
        }
    }
}
