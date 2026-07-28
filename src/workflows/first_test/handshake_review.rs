//! Step 8: the human-supervised submission leg of the full task pipeline.
//!
//! Runs after steps 1-7 (its dependency chain) have downloaded the task,
//! judged it with Claude, and clicked the answers into the multimango page --
//! with the chain's `defer_submit` input set, step 7 leaves Submit untouched.
//! This workflow then:
//!
//! 1. switches to the Handshake task tab (`ai.joinhandshake.com/.../task/<uuid>/run`),
//!    -- the "Open Multimango" busywork now leads the chain as workflow 0,
//! 2. walks the page's chat-style wizard in this order: "Continue" if shown,
//!    the arena task type (derived from the multimango tab's URL) sent with
//!    the up-arrow button, any follow-up "Continue" popup, "Continue task",
//!    and "I submitted my time on Multimango". The wizard renders sent
//!    answers as right-aligned bubbles (`items-end` containers), which is how
//!    "already answered" is detected -- so a step the user pre-clicked by hand
//!    is skipped rather than answered twice, and every step is skipped cleanly
//!    on a re-run. An older page variant used aria-pressed toggle buttons;
//!    both layouts are handled,
//! 4. raises the "GOLEM NEEDS YOU" prompt and BLOCKS until the human reviewer
//!    either signs off, sends feedback to Claude (re-judge + re-apply, then
//!    ask again), or aborts -- UNLESS Settings > Task pipeline > automatic
//!    mode is on, in which case the gate self-approves (see below),
//! 5. only after sign-off: waits for the Handshake task timer to reach the
//!    expected handle time (~35-40 min), re-verifies the answers are still on
//!    the page, and submits on multimango,
//! 6. back on Handshake: "Submit task", then "Confirm time", then "Next task",
//! 7. queues the next pipeline round, which restarts at workflow 0.
//!
//! SAFEGUARDS -- nothing can submit before the review loop returns `Approved`:
//! - step 7 never touches Submit when `defer_submit` is set (the pipeline
//!   always sets it);
//! - every submit call in this file lives AFTER the review loop returns
//!   `Approved`; feedback and abort paths loop or bail out before them;
//! - the answers are re-verified against the live page right before the
//!   multimango submit (the platform can swap the open task under us);
//! - Stop discards the queued next round, so stopping ends the whole loop.
//!
//! AUTOMATIC MODE weakens exactly one of those: `Approved` starts coming from
//! the flag instead of a person, so evaluations are submitted for real with
//! nobody having read them. Every other safeguard still holds -- the answers
//! are still re-verified against the live page, and Stop still ends the loop.

use crate::prelude::*;

use super::util;

pub struct HandshakeReviewAndSubmit;

/// What the reviewer chose in the "GOLEM NEEDS YOU" prompt.
enum Review {
    Approved,
    Aborted,
}

#[async_trait]
impl Workflow for HandshakeReviewAndSubmit {
    fn name(&self) -> &'static str {
        "8. Handshake review + submit (pipeline)"
    }

    fn description(&self) -> &'static str {
        "Full pipeline leg: Handshake busywork, human review gate (nothing submits before \
         sign-off), timed multimango + Handshake submission, then queues the next round."
    }

    fn dependencies(&self) -> Vec<&'static str> {
        vec!["7. Answer + apply evaluation criteria"]
    }

    fn inputs(&self) -> Vec<InputSpec> {
        vec![
            InputSpec::optional("task_dir", "Task folder name (blank = same as step 1)", ""),
            // Shared with every workflow in the chain: tells step 7 to leave
            // Submit to this workflow. Clear it only to run step 7 standalone.
            InputSpec::optional(
                "defer_submit",
                "Leave non-empty so step 7 defers submission to the review gate",
                "yes",
            ),
            InputSpec::optional(
                "target_minutes",
                "Timer minutes to reach before submitting (35-40)",
                "35",
            ),
            InputSpec::optional(
                "loop_pipeline",
                "Non-empty = queue the next round after Continue task",
                "yes",
            ),
            InputSpec::optional(
                "tasks",
                "How many tasks to run in total (blank or 0 = unlimited)",
                "0",
            ),
        ]
    }

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        let timeout = Duration::from_millis(ctx.settings.default_wait_timeout_ms);
        let task_dir = util::current_task_dir(ctx)?;

        // ---- locate the two platform tabs -------------------------------
        ctx.step("locate the multimango + Handshake tabs").await?;
        // We are currently controlling the multimango tab (steps 1-7 ran on
        // it). Its URL names the arena task type Handshake asks about.
        let mm_url = ctx.browser.current_url().await.unwrap_or_default();
        let arena_id = arena_id_from_url(&mm_url).ok_or_else(|| {
            ctx.halt(format!(
                "the controlled tab isn't on a multimango task page (url: {mm_url}); \
                 run the pipeline from the multimango arena tab"
            ))
        })?;
        ctx.output(format!("arena task type: {arena_id}"));
        let hs_tabs = ctx.browser.list_targets("ai.joinhandshake.com").await?;
        if hs_tabs.is_empty() {
            return Err(util::halt_unless_auto(
                ctx,
                "no Handshake tab found -- open ai.joinhandshake.com's task page (the \
                 .../task/<uuid>/run URL) in a tab next to multimango, then re-run.",
            )
            .await);
        }

        // ---- switch to the Handshake task page --------------------------
        ctx.step("switch to the Handshake task page").await?;
        if !ctx
            .browser
            .switch_to_target("ai.joinhandshake.com", "", timeout)
            .await?
        {
            return Err(ctx.halt("couldn't switch to the Handshake tab"));
        }
        util::focus_and_settle(ctx).await?;
        let hs_url = ctx.browser.current_url().await.unwrap_or_default();
        if !(hs_url.contains("/task/") && hs_url.contains("/run")) {
            return Err(util::halt_unless_auto(
                ctx,
                format!(
                    "the Handshake tab is at {hs_url}, not a task run page \
                     (.../annotations/fellow/task/<uuid>/run). Claim/open the task there, \
                     then re-run this workflow."
                ),
            )
            .await);
        }

        // The Open Multimango busywork now leads the chain as workflow 0, so
        // that the round is worked in the right tab from the start.

        // ---- wizard: a Continue step may gate the task-type question ----
        ctx.step("advance the wizard (Continue, if shown)").await?;
        match wait_for_coords(ctx, CONTINUE_STEP_JS, Duration::from_secs(6)).await? {
            Some((x, y)) => {
                let (x, y) = util::jittered(ctx, x, y);
                ctx.click_at_cursor(x, y).await?;
                ctx.output("clicked Continue");
            }
            None => ctx.output("no Continue to click -- moving on"),
        }

        // ---- select the arena task type ---------------------------------
        ctx.step("select the task type on Handshake").await?;
        if answer_wizard_step(ctx, &regex_escape(&arena_id)).await? {
            ctx.output(format!("selected task type: {arena_id}"));
        } else {
            dump_buttons(ctx).await;
            // Hand it to the human instead of halting: they select + send it
            // on the page, dismiss the message, and the pipeline continues.
            util::warn_user_unless_auto(
                ctx,
                format!(
                    "Couldn't find/select the '{arena_id}' task-type option on the \
                     Handshake page (the visible buttons were just logged). Select it \
                     and send it by hand, then dismiss this message to continue."
                ),
            )
            .await?;
        }

        // a confirmation popup with its own Continue may follow the selection
        match wait_for_coords(ctx, CONTINUE_STEP_JS, Duration::from_secs(8)).await? {
            Some((x, y)) => {
                let (x, y) = util::jittered(ctx, x, y);
                ctx.click_at_cursor(x, y).await?;
                ctx.output("clicked Continue on the follow-up popup");
            }
            None => {}
        }

        // ---- wizard: "Continue task" ------------------------------------
        // A miss is fine: the control is also absent when it has already been
        // pressed by hand, which is the same state we want to end up in.
        ctx.step("click Continue task").await?;
        click_button_by_text(ctx, "^continue task$", "Continue task", Duration::from_secs(10))
            .await?;

        // ---- wizard: "I submitted my time on Multimango" ----------------
        // Answered BEFORE the multimango submit: it's a wizard step that gates
        // the rest of the Handshake flow, not a claim the submit already ran.
        // `answer_wizard_step` no-ops when the bubble is already sent, so a
        // hand-clicked step is skipped rather than double-answered.
        ctx.step("answer 'I submitted my time on Multimango'").await?;
        if answer_wizard_step(ctx, "i submitted my (task|time) on multimango").await? {
            ctx.output("confirmed 'I submitted my time on Multimango'");
        } else {
            dump_buttons(ctx).await;
            util::warn_user_unless_auto(
                ctx,
                "Couldn't click the 'I submitted my time on Multimango' option on the \
                 Handshake page (the visible buttons were just logged). Click and send \
                 it by hand, then dismiss this message to continue.",
            )
            .await?;
        }

        // ---- THE HUMAN GATE ---------------------------------------------
        // No submit of any kind happens until this loop returns Approved.
        ctx.step("GOLEM NEEDS YOU -- human review").await?;
        match review_loop(ctx, &task_dir).await? {
            Review::Aborted => {
                // Leave everything as-is (answers applied, nothing submitted)
                // and put the controlled page back on multimango for whatever
                // the human does next.
                let _ = ctx
                    .browser
                    .switch_to_target("multimango.com", "", timeout)
                    .await;
                return Err(ctx.halt(
                    "review aborted by the human reviewer -- NOTHING was submitted. \
                     The applied answers are still on the multimango page.",
                ));
            }
            Review::Approved => {
                ctx.output("human sign-off received");
            }
        }

        // ---- wait out the task timer ------------------------------------
        ctx.step("wait for the Handshake task timer (~35-40 min)").await?;
        let target_minutes: u64 = ctx
            .input("target_minutes")
            .and_then(|v| v.trim().parse().ok())
            .filter(|m| (1..=120).contains(m))
            .unwrap_or(35);
        wait_for_timer(ctx, timeout, target_minutes).await?;

        // ---- submit the evaluation on multimango ------------------------
        ctx.step("submit the evaluation on multimango").await?;
        if !ctx
            .browser
            .switch_to_target("multimango.com", "", timeout)
            .await?
        {
            return Err(ctx.halt("couldn't switch back to the multimango tab"));
        }
        util::focus_and_settle(ctx).await?;
        // Safeguard: the platform can swap the open task while we were in
        // review -- submitting then would rate a DIFFERENT task. Verify every
        // approved answer is still selected; re-apply once if not.
        let answers = util::read_claude_answers(&task_dir.join("claude_answers"))?;
        let mut wrong = util::verify_answers_applied(ctx, &answers).await?;
        if !wrong.is_empty() {
            ctx.warn(format!(
                "{} answer(s) no longer selected on the page (task may have re-rendered) -- \
                 re-applying once",
                wrong.len()
            ));
            let _ = util::apply_answers(ctx, &answers, false).await?;
            wrong = util::verify_answers_applied(ctx, &answers).await?;
        }
        if !wrong.is_empty() {
            return Err(util::halt_unless_auto(
                ctx,
                format!(
                    "even after re-applying, these answers aren't selected: {} -- the open \
                     multimango task probably CHANGED since the review. NOT submitting; \
                     check the page.",
                    wrong.join(", ")
                ),
            )
            .await);
        }
        if !util::click_submit_if_enabled(ctx).await? {
            return Err(util::halt_unless_auto(
                ctx,
                "the multimango Submit button wasn't found or never enabled -- \
                 submit by hand, then continue on the Handshake side manually.",
            )
            .await);
        }
        ctx.output("multimango evaluation submitted");

        // ---- submit on Handshake ----------------------------------------
        ctx.step("submit on Handshake").await?;
        if !ctx
            .browser
            .switch_to_target("ai.joinhandshake.com", "", timeout)
            .await?
        {
            return Err(ctx.halt("couldn't switch back to the Handshake tab"));
        }
        util::focus_and_settle(ctx).await?;
        // "I submitted my time on Multimango" was already answered before the
        // review gate -- all that's left here is the final submit.
        if !util::click_submit_with(ctx, HANDSHAKE_SUBMIT_JS).await? {
            return Err(util::halt_unless_auto(
                ctx,
                "the Handshake Submit button wasn't found or never enabled (is the \
                 task-type still selected?). Submit by hand.",
            )
            .await);
        }
        ctx.output("Handshake task submitted");

        // Handshake asks to confirm the handle time before releasing the task.
        ctx.step("confirm the time").await?;
        click_button_by_text(ctx, "confirm.*time", "Confirm time", Duration::from_secs(15)).await?;

        // ---- move to the next task + queue the next round ---------------
        ctx.step("go to the next task").await?;
        let prev_run_url = hs_url;
        if !click_button_by_text(ctx, "next task", "Next task", Duration::from_secs(30)).await? {
            util::warn_user_unless_auto(
                ctx,
                "couldn't find a 'Next task' button after submitting. Click it yourself \
                 (or claim the next task), then dismiss this message.",
            )
            .await?;
        }
        // A new round only makes sense if Handshake actually moved to a new
        // task run page.
        let mut on_new_task = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while tokio::time::Instant::now() < deadline {
            ctx.guard().await?;
            let url = ctx.browser.current_url().await.unwrap_or_default();
            if url.contains("/task/") && url.contains("/run") && url != prev_run_url {
                on_new_task = true;
                break;
            }
            ctx.human_pause(600, 1000).await?;
        }

        // Whatever happens next, the next round's steps 1-7 drive multimango.
        let _ = ctx
            .browser
            .switch_to_target("multimango.com", "", timeout)
            .await;

        let loop_pipeline = ctx
            .input("loop_pipeline")
            .is_some_and(|v| !v.trim().is_empty());
        // How many tasks the user asked for, and how many rounds have finished
        // (this one included). `tasks` is the user-facing total and rides along
        // unchanged; `tasks_done` is bookkeeping this workflow queues to itself.
        // 0/blank/unparseable = unlimited, which is the historical behaviour.
        let tasks_total = parse_count(ctx.input("tasks"));
        let tasks_done = parse_count(ctx.input("tasks_done")).saturating_add(1);
        let budget_left = tasks_total == 0 || tasks_done < tasks_total;
        if on_new_task && loop_pipeline && budget_left {
            let mut inputs = std::collections::BTreeMap::new();
            // task_dir stays empty so step 1 auto-creates the next taskN.
            inputs.insert("defer_submit".to_string(), "yes".to_string());
            inputs.insert(
                "target_minutes".to_string(),
                target_minutes.to_string(),
            );
            inputs.insert("loop_pipeline".to_string(), "yes".to_string());
            inputs.insert("tasks".to_string(), tasks_total.to_string());
            inputs.insert("tasks_done".to_string(), tasks_done.to_string());
            ctx.queue_chain(vec![self.name().to_string()], inputs);
            if tasks_total == 0 {
                ctx.output(format!(
                    "task {tasks_done} done -- next round queued (unlimited; press Stop to \
                     end the loop)."
                ));
            } else {
                ctx.output(format!(
                    "task {tasks_done} of {tasks_total} done -- next round queued (press Stop \
                     to end the loop early)."
                ));
            }
        } else if on_new_task && loop_pipeline && !budget_left {
            ctx.output(format!(
                "ran all {tasks_total} requested task(s) -- not queueing another round. \
                 Raise 'How many tasks to run' (or set it to 0) to keep going."
            ));
        } else if !on_new_task {
            ctx.warn(
                "Handshake didn't move to a new task run page -- not queueing another \
                 round. Start the next pipeline manually when ready.",
            );
        } else {
            ctx.output("loop_pipeline is off -- not queueing another round.");
        }

        Ok(WorkflowOutcome::Completed)
    }
}

// ---------------------------------------------------------------------------
// review loop
// ---------------------------------------------------------------------------

/// The "GOLEM NEEDS YOU" gate. Loops until the reviewer approves or aborts;
/// the feedback branch re-runs Claude with the reviewer's note, re-applies
/// the (new) answers on multimango, and asks again.
async fn review_loop(ctx: &mut WorkflowCtx, task_dir: &std::path::Path) -> Result<Review> {
    let timeout = Duration::from_millis(ctx.settings.default_wait_timeout_ms);
    // Automatic mode: nobody is going to look, so approve what Claude wrote and
    // let the caller submit it. The answers are already applied and verified on
    // the page at this point; this skips only the human's sign-off.
    if util::auto_mode(ctx) {
        ctx.warn(
            "automatic mode: skipping the GOLEM NEEDS YOU review -- submitting Claude's \
             evaluation with nobody having checked it.",
        );
        return Ok(Review::Approved);
    }
    loop {
        ctx.guard().await?;
        let choice = ctx
            .choose(
                "GOLEM NEEDS YOU\n\nReview the evaluation applied on the multimango tab \
                 (answers + overall pick). NOTHING has been submitted yet.\n\nThe task \
                 timer keeps running during review.",
                vec![
                    "OK -- everything is right, submit it".to_string(),
                    "Ask Claude to fix something (give feedback)".to_string(),
                    "Abort -- leave everything unsubmitted".to_string(),
                ],
            )
            .await;
        match choice {
            Ok(0) => return Ok(Review::Approved),
            Ok(1) => {
                let feedback = match ctx
                    .prompt_text(
                        "What should Claude fix or reconsider? (be specific: which \
                         criterion / which response / what's wrong)",
                        "",
                    )
                    .await
                {
                    Ok(text) if !text.trim().is_empty() => text,
                    _ => {
                        ctx.output("no feedback entered -- back to the review prompt");
                        continue;
                    }
                };
                // Re-judge with the reviewer's note, then re-apply on the
                // multimango tab (the applier brings it to the front).
                if !ctx
                    .browser
                    .switch_to_target("multimango.com", "", timeout)
                    .await?
                {
                    ctx.warn("couldn't switch to the multimango tab -- try again");
                    continue;
                }
                ctx.output("asking Claude to revise the evaluation with your feedback...");
                if let Err(e) = util::ask_claude_for_answers(ctx, task_dir, Some(&feedback)).await
                {
                    ctx.warn(format!("claude failed to revise ({e}) -- review again"));
                    continue;
                }
                match util::read_claude_answers(&task_dir.join("claude_answers")) {
                    Ok(answers) => {
                        let (applied, missed) = util::apply_answers(ctx, &answers, false).await?;
                        ctx.output(format!(
                            "re-applied {applied} button(s){}",
                            if missed.is_empty() {
                                String::new()
                            } else {
                                format!(" ({} missed: {})", missed.len(), missed.join(", "))
                            }
                        ));
                    }
                    Err(e) => {
                        ctx.warn(format!("couldn't parse the revised answers ({e})"));
                    }
                }
                // Back to the top: the reviewer looks again.
            }
            Ok(_) => return Ok(Review::Aborted),
            // Prompt dismissed/cancelled (or Stop): treat as abort -- never
            // fall through toward the submit path on an unclear answer.
            Err(GolemError::StoppedByUser) => return Err(GolemError::StoppedByUser),
            Err(_) => return Ok(Review::Aborted),
        }
    }
}

// ---------------------------------------------------------------------------
// timer
// ---------------------------------------------------------------------------

/// Read the Handshake task timer and wait until it reaches `target_minutes`
/// (+ up to ~3 min of jitter, capped at 40). If the timer is paused, resume
/// it first; if it can't be read at all, fall back to asking the human to
/// watch it. The project expects handle times near the human average -- the
/// timer only exists so tasks aren't claimed implausibly fast.
async fn wait_for_timer(
    ctx: &mut WorkflowCtx,
    switch_timeout: Duration,
    target_minutes: u64,
) -> Result<()> {
    if !ctx
        .browser
        .switch_to_target("ai.joinhandshake.com", "", switch_timeout)
        .await?
    {
        return Err(ctx.halt("couldn't switch to the Handshake tab to read the timer"));
    }
    // Jitter the target inside the 35-40 window so rounds don't all land on
    // the exact same handle time.
    let jitter = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_millis() as u64)
        .unwrap_or(0))
        % 180;
    let target_secs = (target_minutes * 60 + jitter).min(40 * 60);

    let mut last_report: Option<u64> = None;
    loop {
        ctx.guard().await?;
        let v = ctx.eval(TIMER_READ_JS).await?;
        let secs = v.get("secs").and_then(Value::as_u64);
        let running = v.get("running").and_then(Value::as_bool).unwrap_or(true);
        match secs {
            None => {
                // Can't read it -- hand the judgment to the human rather than
                // submitting at an implausible handle time.
                if util::auto_mode(ctx) {
                    // Nobody to watch it, and submitting now would post an
                    // implausible handle time. Wait the target out by wall
                    // clock instead: the real task timer started before this
                    // point, so this over-waits, which is the safe direction
                    // to be wrong in.
                    ctx.warn(format!(
                        "automatic mode: can't read the task timer, so waiting the full \
                         {target_minutes} min by wall clock before submitting (this \
                         over-waits -- the task timer started earlier)."
                    ));
                    let until = tokio::time::Instant::now() + Duration::from_secs(target_secs);
                    while tokio::time::Instant::now() < until {
                        ctx.guard().await?;
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                    return Ok(());
                }
                ctx.warn_user(format!(
                    "Couldn't read the task timer on the Handshake page. Watch it \
                     yourself and dismiss this message once it shows at least \
                     {target_minutes} minutes -- submission continues after that.",
                ))
                .await?;
                return Ok(());
            }
            Some(s) if s >= target_secs => {
                ctx.output(format!(
                    "timer at {}:{:02} -- proceeding to submit",
                    s / 60,
                    s % 60
                ));
                return Ok(());
            }
            Some(s) => {
                if !running {
                    // A paused timer never reaches the target; resume it.
                    if let (Some(x), Some(y)) = (
                        v.get("bx").and_then(Value::as_f64),
                        v.get("by").and_then(Value::as_f64),
                    ) {
                        ctx.output("task timer is paused -- resuming it");
                        let (x, y) = util::jittered(ctx, x, y);
                        ctx.click_at_cursor(x, y).await?;
                    }
                }
                // Progress note roughly once a minute.
                if last_report.is_none_or(|r| s.saturating_sub(r) >= 60) {
                    ctx.output(format!(
                        "timer at {}:{:02}, waiting for {}:{:02} before submitting",
                        s / 60,
                        s % 60,
                        target_secs / 60,
                        target_secs % 60
                    ));
                    last_report = Some(s);
                }
                ctx.human_pause(5000, 9000).await?;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// small helpers + page finders
// ---------------------------------------------------------------------------

/// A round-counting input (`tasks` / `tasks_done`) as a plain count. Blank,
/// missing and unparseable all mean 0, which the caller reads as "unlimited"
/// -- a typo in the box must never silently cut the run short.
fn parse_count(raw: Option<&str>) -> u32 {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0)
}

/// `https://www.multimango.com/tasks/<arena-id>` -> `<arena-id>`.
fn arena_id_from_url(url: &str) -> Option<String> {
    let rest = url.split("multimango.com/tasks/").nth(1)?;
    let id = rest
        .split(|c| c == '/' || c == '?' || c == '#')
        .next()
        .unwrap_or("");
    (!id.is_empty()).then(|| id.to_string())
}

/// Quote a Rust string as a JS string literal.
fn js_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// Click a plain Handshake button matched by its text.
///
/// Returns whether it was found and clicked. A miss is NOT an error: the same
/// control is absent when the step has already been taken -- including when
/// the user pressed it by hand before Golem got there -- and skipping is
/// exactly the right response to that.
async fn click_button_by_text(
    ctx: &mut WorkflowCtx,
    pattern: &str,
    label: &str,
    timeout: Duration,
) -> Result<bool> {
    let find_js = BUTTON_BY_TEXT_JS.replace("__PATTERN__", &js_str(pattern));
    match wait_for_coords(ctx, &find_js, timeout).await? {
        Some((x, y)) => {
            let (x, y) = util::jittered(ctx, x, y);
            ctx.click_at_cursor(x, y).await?;
            ctx.output(format!("clicked '{label}'"));
            Ok(true)
        }
        None => {
            ctx.output(format!(
                "no '{label}' control visible -- skipping (already pressed, or this state \
                 doesn't show one)"
            ));
            Ok(false)
        }
    }
}

/// Poll `find_js` (an IIFE returning `{x, y}` or null) until it yields
/// coordinates or `timeout` passes.
pub(super) async fn wait_for_coords(
    ctx: &mut WorkflowCtx,
    find_js: &str,
    timeout: Duration,
) -> Result<Option<(f64, f64)>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        ctx.guard().await?;
        let v = ctx.eval(find_js).await?;
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

/// Escape a literal string for embedding in a JS regex source.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\^$.|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Answer one step of Handshake's chat-style wizard: the option whose text
/// matches `pattern` (a case-insensitive JS regex matched against the whole
/// trimmed text) is clicked, then sent with the wizard's up-arrow button if
/// one appears, and the answer is confirmed by its sent bubble showing up in
/// the user-side (`items-end`) column. If the answer bubble is already there
/// (step answered on a previous run, or by hand), this does nothing and
/// reports success. The older toggle-button layout (selection state in
/// `aria-pressed`) is handled by the same finder. Returns `false` if the
/// option never appeared or the answer never registered.
async fn answer_wizard_step(ctx: &mut WorkflowCtx, pattern: &str) -> Result<bool> {
    let find_js = WIZARD_ANSWER_JS.replace("__PATTERN__", &js_str(pattern));
    // The step (or its already-sent bubble) may render late.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        ctx.guard().await?;
        let v = ctx.eval(&find_js).await?;
        if v.get("x").and_then(Value::as_f64).is_some() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        ctx.human_pause(300, 600).await?;
    }
    for _attempt in 0..2 {
        let v = ctx.eval(&find_js).await?;
        let (Some(x), Some(y)) = (
            v.get("x").and_then(Value::as_f64),
            v.get("y").and_then(Value::as_f64),
        ) else {
            return Ok(false);
        };
        if v.get("selected").and_then(Value::as_bool).unwrap_or(false) {
            return Ok(true);
        }
        let (x, y) = util::jittered(ctx, x, y);
        ctx.click_at_cursor(x, y).await?;
        ctx.human_pause(400, 900).await?;
        // The chat layout needs the choice sent with the up-arrow button;
        // the toggle layout has no such button and registers on the click.
        if let Some((sx, sy)) = wait_for_coords(ctx, WIZARD_SEND_JS, Duration::from_secs(4)).await?
        {
            let (sx, sy) = util::jittered(ctx, sx, sy);
            ctx.click_at_cursor(sx, sy).await?;
        }
        let confirm_deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while tokio::time::Instant::now() < confirm_deadline {
            ctx.guard().await?;
            let v = ctx.eval(&find_js).await?;
            if v.get("selected").and_then(Value::as_bool).unwrap_or(false) {
                return Ok(true);
            }
            ctx.human_pause(400, 700).await?;
        }
    }
    let v = ctx.eval(&find_js).await?;
    Ok(v.get("selected").and_then(Value::as_bool).unwrap_or(false))
}

/// Log the visible buttons on the page -- called when a finder misses so the
/// mismatch is debuggable from the output pane alone.
async fn dump_buttons(ctx: &mut WorkflowCtx) {
    if let Ok(v) = ctx.eval(DUMP_BUTTONS_JS).await
        && let Some(s) = v.as_str()
    {
        ctx.output(format!("visible buttons on the page: {s}"));
    }
}

/// A visible "Open Multimango" button/link (shows in Handshake's task guide /
/// return-reminder dialogs).
/// A visible, enabled button/link/chip whose trimmed text matches
/// `__PATTERN__` (a case-insensitive JS regex source). The LAST match in
/// document order wins, which in the chat wizard is the newest step. Backs
/// [`click_button_by_text`] for the plain Handshake controls that aren't
/// wizard chips: Continue task, Confirm time, Next task.
const BUTTON_BY_TEXT_JS: &str = r#"(function(){
  var re;
  try { re = new RegExp(__PATTERN__, 'i'); } catch (e) { return null; }
  var els = document.querySelectorAll('button, a, [role="button"]');
  var best = null;
  for (var i = 0; i < els.length; i++) {
    var t = (els[i].textContent || '').trim();
    if (!t || !re.test(t)) continue;
    if (els[i].disabled || els[i].getAttribute('aria-disabled') === 'true') continue;
    var r = els[i].getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    best = els[i];
  }
  if (!best) return null;
  try { best.scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
  var br = best.getBoundingClientRect();
  if (br.width < 1 || br.height < 1) return null;
  return { x: br.left + br.width / 2, y: br.top + br.height / 2 };
})()"#;

/// The control that opens Multimango in a NEW tab. Handshake shows it in two
/// different shapes depending on page state, and only one exists at a time:
///
/// - an explicit `<button>Open Multimango</button>`, which lives in the modal
///   "Timer paused" dialog (see `Downloads/Handshake AI.html`);
/// - the inline `<a target="_blank">Multimango</a>` links in the task's
///   "⚠️ Important" instruction list, which is all that's present in the normal
///   task state (see `Downloads/HEREIS.html`) -- their href is
///   `multimango.com/sign-in?email={multimango_credentials}`, an unsubstituted
///   template, so the tab they open is a throwaway.
///
/// The button wins when both are on screen: it's the purpose-built control,
/// while the links are prose. Either way the click opens a tab that workflow 0
/// closes again.
pub(super) const OPEN_MULTIMANGO_JS: &str = r#"(function(){
  var els = document.querySelectorAll('button, a, [role="button"]');
  var button = null, link = null;
  for (var i = 0; i < els.length; i++) {
    var el = els[i];
    if (el.disabled || el.getAttribute('aria-disabled') === 'true') continue;
    var r = el.getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    if ((el.textContent || '').trim().toLowerCase() === 'open multimango') { button = el; break; }
    if (!link && (el.getAttribute('href') || '').toLowerCase().indexOf('multimango.com') !== -1) {
      link = el;
    }
  }
  var best = button || link;
  if (!best) return null;
  try { best.scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
  var br = best.getBoundingClientRect();
  if (br.width < 1 || br.height < 1) return null;
  return { x: br.left + br.width / 2, y: br.top + br.height / 2,
           kind: button ? 'button' : 'link' };
})()"#;

/// One wizard step's answer control, matched by `__PATTERN__` (a JS regex
/// source tested case-insensitively against the whole trimmed text).
/// Handles both page layouts:
/// - toggle buttons (older variant): selection state is `aria-pressed`;
/// - chat wizard (current variant, inspected live 2026-07-27): options are
///   plain styled `<div>` chips, and a SENT answer shows as a bubble inside
///   the user-side column (a `[class*="items-end"]` container) -- that bubble
///   is the `selected` signal. The LAST match in document order wins, which
///   is both the innermost element and the newest wizard step.
const WIZARD_ANSWER_JS: &str = r#"(function(){
  var RE = new RegExp('^(?:' + __PATTERN__ + ')$', 'i');
  function pt(e, sel) {
    try { e.scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
    var r = e.getBoundingClientRect();
    if (r.width < 1 || r.height < 1) return null;
    return { x: r.left + r.width / 2, y: r.top + r.height / 2, selected: sel };
  }
  function vis(e) { var r = e.getBoundingClientRect(); return r.width >= 1 && r.height >= 1; }
  var btns = document.querySelectorAll('button');
  for (var i = 0; i < btns.length; i++) {
    if (!btns[i].hasAttribute('aria-pressed')) continue;
    if (!RE.test((btns[i].textContent || '').trim())) continue;
    if (!vis(btns[i])) continue;
    return pt(btns[i], btns[i].getAttribute('aria-pressed') === 'true');
  }
  var all = document.querySelectorAll('div, span, button, [role="button"], a');
  var answered = null, option = null;
  for (var j = 0; j < all.length; j++) {
    var el = all[j];
    if (!RE.test((el.textContent || '').trim())) continue;
    if (!vis(el)) continue;
    if (el.closest('[class*="items-end"]')) answered = el; else option = el;
  }
  if (answered) return pt(answered, true);
  if (option) return pt(option, false);
  return null;
})()"#;

/// The chat wizard's send button (the up-arrow, lower right of the input
/// area): an enabled button with an arrow-up icon or a send/submit
/// aria-label. Bottom-most match wins.
const WIZARD_SEND_JS: &str = r#"(function(){
  var btns = document.querySelectorAll('button');
  var best = null, bestTop = -1;
  for (var i = 0; i < btns.length; i++) {
    var b = btns[i];
    if (b.disabled || b.getAttribute('aria-disabled') === 'true') continue;
    var al = (b.getAttribute('aria-label') || '').trim().toLowerCase();
    var arrow = b.querySelector('svg[class*="arrow-up"]');
    if (!arrow && al !== 'send' && al !== 'send message' && al !== 'submit') continue;
    var r = b.getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    if (r.top > bestTop) { bestTop = r.top; best = b; }
  }
  if (!best) return null;
  try { best.scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
  var r = best.getBoundingClientRect();
  return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
})()"#;

/// A pending "Continue" control in the wizard or a popup: button, link, or
/// chat option chip -- but never an already-sent answer bubble (user-side
/// `items-end` column). Last match in document order = the newest step.
const CONTINUE_STEP_JS: &str = r#"(function(){
  var els = document.querySelectorAll('button, [role="button"], a, div');
  var best = null;
  for (var i = 0; i < els.length; i++) {
    var el = els[i];
    var t = (el.textContent || '').trim().toLowerCase();
    if (t !== 'continue' && t !== 'continue to task') continue;
    if (el.disabled || el.getAttribute('aria-disabled') === 'true') continue;
    if (el.closest('[class*="items-end"]')) continue;
    var r = el.getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    best = el;
  }
  if (!best) return null;
  try { best.scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
  var r = best.getBoundingClientRect();
  return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
})()"#;

/// Handshake's final submit: an enabled button reading "Submit task" (the
/// big bottom button on the live page, `type="button"`) or plain "Submit"
/// (older variant, `type="submit"`).
const HANDSHAKE_SUBMIT_JS: &str = r#"(function(){
  var btns = document.querySelectorAll('button');
  for (var i = 0; i < btns.length; i++) {
    var b = btns[i];
    var al = (b.getAttribute('aria-label') || '').trim().toLowerCase();
    var txt = (b.textContent || '').trim().toLowerCase();
    if (al !== 'submit' && txt !== 'submit' && txt !== 'submit task') continue;
    if (b.disabled || b.getAttribute('aria-disabled') === 'true') continue;
    try { b.scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
    var r = b.getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
  }
  return null;
})()"#;

/// The task timer: a button whose aria-label mentions "timer" ("Pause timer"
/// while running), with the elapsed H:MM:SS / MM:SS in a nearby ancestor's
/// text. Falls back to the document title ("MM:SS - Handshake AI"). Returns
/// `{secs, running, bx, by}` (bx/by = the timer button, for resuming a
/// paused timer) or null.
const TIMER_READ_JS: &str = r#"(function(){
  function parse(t) {
    var m = (t || '').match(/(\d{1,2}):(\d{2})(?::(\d{2}))?/);
    if (!m) return null;
    return m[3] !== undefined
      ? (+m[1]) * 3600 + (+m[2]) * 60 + (+m[3])
      : (+m[1]) * 60 + (+m[2]);
  }
  var btns = document.querySelectorAll('button[aria-label]');
  for (var i = 0; i < btns.length; i++) {
    var al = (btns[i].getAttribute('aria-label') || '').toLowerCase();
    if (al.indexOf('timer') === -1) continue;
    var running = al.indexOf('pause') !== -1;
    var box = btns[i].parentElement, secs = null;
    for (var up = 0; up < 4 && box; up++) {
      secs = parse(box.textContent || '');
      if (secs !== null) break;
      box = box.parentElement;
    }
    if (!running) {
      try { btns[i].scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
    }
    var r = btns[i].getBoundingClientRect();
    return { secs: secs, running: running,
             bx: r.left + r.width / 2, by: r.top + r.height / 2 };
  }
  var t = parse(document.title);
  return t === null ? null : { secs: t, running: true, bx: null, by: null };
})()"#;

/// Every visible button's text, for diagnostics when a finder misses.
const DUMP_BUTTONS_JS: &str = r#"(function(){
  var btns = document.querySelectorAll('button, [role="button"]');
  var out = [];
  for (var i = 0; i < btns.length && out.length < 40; i++) {
    var r = btns[i].getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    var t = (btns[i].textContent || '').trim().slice(0, 50);
    if (t) out.push(t);
  }
  return out.join(' | ');
})()"#;
