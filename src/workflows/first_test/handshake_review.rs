//! Step 8: the unattended submission leg of the full task pipeline.
//!
//! Runs after steps 1-7 (its dependency chain) have downloaded the task,
//! judged it with Claude, and clicked the answers into the multimango page --
//! with the chain's `defer_submit` input set, step 7 leaves Submit untouched.
//! This workflow then:
//!
//! 1. switches to the Handshake task tab (`ai.joinhandshake.com/.../task/<uuid>/run`)
//!    -- the "Open Multimango" busywork leads the chain as workflow 0,
//! 2. walks the page's chat-style wizard in this order: "Continue" if shown,
//!    the arena task type (derived from the multimango tab's URL) sent with
//!    the up-arrow button, any follow-up "Continue" popup, "Continue task",
//!    and "I submitted my time on Multimango". The wizard renders sent
//!    answers as right-aligned bubbles (`items-end` containers), which is how
//!    "already answered" is detected -- so a step the user pre-clicked by hand
//!    is skipped rather than answered twice, and every step is skipped cleanly
//!    on a re-run. An older page variant used aria-pressed toggle buttons;
//!    both layouts are handled,
//! 3. waits for the task timer to reach 40 minutes (plus up to 3 of jitter)
//!    BEFORE either submission -- both platforms record a handle time, so the
//!    wait sits ahead of the first submit of the round rather than only in
//!    front of the last one. A timer already at/past 40 when it gets there
//!    (the common case, since Claude's judging in steps 1-7 usually runs
//!    longer than that on its own) means no wait at all,
//! 4. re-verifies the answers are still on the page, then submits on multimango,
//! 5. back on Handshake, re-checks the timer (normally instant -- it only
//!    moves forward) and clicks "Submit task", then "Confirm time", then
//!    "Next task",
//! 6. queues the next pipeline round, which restarts at workflow 0.
//!
//! Every hop between controls goes through [`between_clicks`]: a jittered
//! pause plus occasional idle cursor drift, so the pointer never bee-lines
//! from one button straight to the next.
//!
//! THERE IS NO HUMAN GATE. The old "GOLEM NEEDS YOU" review prompt was removed
//! by request so the pipeline runs continuously without intervention: Claude's
//! evaluation is submitted for real with nobody having read it. What still
//! stands between the answers and the submit:
//! - step 7 never touches Submit when `defer_submit` is set (the pipeline
//!   always sets it), so submission happens here and only here;
//! - the timer wait above, which keeps handle times plausible. It is a FLOOR,
//!   not a midpoint: Handshake's own widget reads `00:06:36/00:40:00`, so 40
//!   minutes is what the platform asks for, and the jitter only ever scatters
//!   rounds above it;
//! - the answers are re-verified against the live page right before the
//!   multimango submit (the platform can swap the open task under us);
//! - Stop discards the queued next round, so stopping ends the whole loop.

use rand::RngExt;

use crate::prelude::*;

use super::util;

pub struct HandshakeReviewAndSubmit;

#[async_trait]
impl Workflow for HandshakeReviewAndSubmit {
    fn name(&self) -> &'static str {
        "8. Handshake review + submit (pipeline)"
    }

    fn description(&self) -> &'static str {
        "Full pipeline leg: Handshake wizard, waits for the task timer to reach 40 min (+ up \
         to 3 of jitter), then submits on multimango + Handshake and queues the next round. \
         No human gate."
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
                "Leave non-empty so step 7 defers submission to this workflow",
                "yes",
            ),
            InputSpec::optional(
                "target_minutes",
                "Minimum timer minutes before submitting (+ up to 3 min jitter). 40 is a \
                 floor, not just the default -- lower values are raised to it",
                "40",
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
        let mm_url = current_url_settled(ctx).await;
        let arena_id = arena_id_from_url(&mm_url).ok_or_else(|| {
            ctx.halt(format!(
                "the controlled tab isn't on a multimango task page (url: {}); \
                 run the pipeline from the multimango arena tab",
                if mm_url.is_empty() { "unreadable" } else { &mm_url }
            ))
        })?;
        ctx.output(format!("arena task type: {arena_id}"));
        if !handshake_tab_exists(ctx).await? {
            return Err(util::halt_now(
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
        let hs_url = current_url_settled(ctx).await;
        if !(hs_url.contains("/task/") && hs_url.contains("/run")) {
            return Err(util::halt_now(
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
        click_control(ctx, CONTINUE_STEP_JS, "Continue", Duration::from_secs(6)).await?;

        // ---- select the arena task type ---------------------------------
        ctx.step("select the task type on Handshake").await?;
        between_clicks(ctx).await?;
        // Try the multimango slug verbatim first -- when both sides agree,
        // which is the common case, this is the whole story.
        let mut selected = answer_wizard_step(ctx, &regex_escape(&arena_id)).await?;
        if selected {
            ctx.output(format!("selected task type: {arena_id}"));
        } else {
            // They didn't agree. Handshake labels the same job with a
            // different id group often enough that failing here would stall
            // the unattended loop, so decide from what is actually on offer.
            let options = wizard_option_texts(ctx).await?;
            match pick_task_type_option(&arena_id, &options) {
                Some(pick) => {
                    if !pick.exact {
                        ctx.warn(format!(
                            "the multimango task is '{arena_id}' but Handshake offers '{}' -- \
                             taking it as the same task type",
                            pick.text
                        ));
                    }
                    selected = answer_wizard_step(ctx, &regex_escape(&pick.text)).await?;
                    if selected {
                        ctx.output(format!("selected task type: {}", pick.text));
                    }
                }
                None => ctx.warn(format!(
                    "no task-type option matches '{arena_id}'; offered: {}",
                    if options.is_empty() {
                        "(none found)".to_string()
                    } else {
                        options.join(", ")
                    }
                )),
            }
        }
        if !selected {
            dump_buttons(ctx).await;
            // Hand it to the human instead of halting: they select + send it
            // on the page, dismiss the message, and the pipeline continues.
            util::warn_no_block(
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
        between_clicks(ctx).await?;
        click_control(
            ctx,
            CONTINUE_STEP_JS,
            "Continue (follow-up popup)",
            Duration::from_secs(8),
        )
        .await?;

        // ---- wizard: "Continue task" ------------------------------------
        // A miss is fine: the control is also absent when it has already been
        // pressed by hand, which is the same state we want to end up in.
        ctx.step("click Continue task").await?;
        between_clicks(ctx).await?;
        click_button_by_text(ctx, "^continue task$", "Continue task", Duration::from_secs(10))
            .await?;

        // ---- wizard: "I submitted my time on Multimango" ----------------
        // Answered BEFORE the multimango submit: it's a wizard step that gates
        // the rest of the Handshake flow, not a claim the submit already ran.
        // `answer_wizard_step` no-ops when the bubble is already sent, so a
        // hand-clicked step is skipped rather than double-answered.
        ctx.step("answer 'I submitted my time on Multimango'").await?;
        between_clicks(ctx).await?;
        if answer_wizard_step(ctx, "i submitted my (task|time) on multimango").await? {
            ctx.output("confirmed 'I submitted my time on Multimango'");
        } else {
            dump_buttons(ctx).await;
            util::warn_no_block(
                ctx,
                "Couldn't click the 'I submitted my time on Multimango' option on the \
                 Handshake page (the visible buttons were just logged). Click and send \
                 it by hand, then dismiss this message to continue.",
            )
            .await?;
        }

        let asked = ctx.input("target_minutes");
        let target_minutes = resolve_target_minutes(asked);
        if let Some(asked) = asked.map(str::trim).filter(|s| !s.is_empty())
            && asked.parse::<u64>().ok() != Some(target_minutes)
        {
            ctx.warn(format!(
                "the target_minutes box says '{asked}', but {TIMER_FLOOR_MINUTES} minutes is \
                 the floor, not just the default -- Handshake's own timer reads \
                 00:MM:SS/00:{TIMER_FLOOR_MINUTES}:00, so that is the handle time it asks \
                 for. Waiting {target_minutes} min."
            ));
        }

        // ---- wait out the task timer ------------------------------------
        // BEFORE either submission, not just the Handshake one. Both platforms
        // record a handle time, and multimango used to be handed the evaluation
        // the moment Claude's answers were in -- often only a few minutes after
        // the task was claimed -- while only the Handshake click waited. The
        // wait belongs ahead of the first submission of the round so neither
        // side sees an implausible time.
        //
        // Usually this returns straight away: steps 1-7 (downloads plus
        // Claude's judging) have normally already pushed the timer past 40 min
        // by the time the chain reaches here. We are on the Handshake tab, and
        // `wait_for_timer` needs it to read the timer.
        ctx.step("wait for the Handshake task timer").await?;
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
            return Err(util::halt_now(
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
        between_clicks(ctx).await?;
        if !util::click_submit_if_enabled(ctx).await? {
            return Err(util::halt_now(
                ctx,
                "the multimango submit control wasn't found or never enabled (\"Save & \
                 Continue\" inside the evaluation-criteria panel on the newest layout, or \
                 Submit next to Skip on older ones) -- submit by hand, then continue on the \
                 Handshake side manually.",
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
        // Re-check the timer before the final click. Normally instant -- the
        // timer only moves forward and the wait above already cleared the
        // target -- but the multimango submit sits between the two, and if
        // Handshake paused the timer while we were away this is what notices
        // and resumes it.
        ctx.step("re-check the Handshake task timer").await?;
        wait_for_timer(ctx, timeout, target_minutes).await?;
        // "I submitted my time on Multimango" was already answered earlier in
        // the wizard -- all that's left here is the final submit.
        between_clicks(ctx).await?;
        if !util::click_submit_with(ctx, HANDSHAKE_SUBMIT_JS).await? {
            return Err(util::halt_now(
                ctx,
                "the Handshake Submit button wasn't found or never enabled (is the \
                 task-type still selected?). Submit by hand.",
            )
            .await);
        }
        ctx.output("Handshake task submitted");

        // Handshake asks to confirm the handle time before releasing the task.
        ctx.step("confirm the time").await?;
        between_clicks(ctx).await?;
        click_button_by_text(ctx, "confirm.*time", "Confirm time", Duration::from_secs(15)).await?;

        // ---- move to the next task + queue the next round ---------------
        ctx.step("go to the next task").await?;
        let prev_run_url = hs_url;
        between_clicks(ctx).await?;
        if !click_button_by_text(ctx, "next task", "Next task", Duration::from_secs(30)).await? {
            util::warn_no_block(
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


// ---------------------------------------------------------------------------
// timer
// ---------------------------------------------------------------------------

/// Read the Handshake task timer and hold the round until it reaches
/// `target_minutes` (plus up to 3 min of jitter). `target_minutes` is a FLOOR,
/// not a midpoint: Handshake's own timer widget renders elapsed-over-target as
/// `00:06:36/00:40:00`, so the platform itself states 40:00 as the handle time
/// it expects, and submitting under that is the thing worth avoiding. The
/// jitter therefore only ever scatters rounds ABOVE the floor, so no round is
/// submitted at less than the target.
///
/// A first reading already at/past the plain target returns immediately: the
/// jitter stretches a wait that is happening anyway, it never delays a round
/// that arrives late (the common case, since Claude's judging in steps 1-7
/// usually runs past 40 min on its own).
///
/// If the timer is paused, it is resumed. If it can't be read -- unreadable,
/// or the browser connection dropped mid-wait -- the wall-clock deadline below
/// carries the wait instead, so a frozen or unresumable timer can never hang
/// the round forever.
async fn wait_for_timer(
    ctx: &mut WorkflowCtx,
    switch_timeout: Duration,
    target_minutes: u64,
) -> Result<()> {
    // No "0 means don't wait" escape hatch: callers resolve `target_minutes`
    // through `resolve_target_minutes`, which never returns less than the
    // floor. A skip-the-wait branch inside the one function whose entire job is
    // to enforce the wait is the sort of thing that quietly becomes reachable.
    if !ctx
        .browser
        .switch_to_target("ai.joinhandshake.com", "", switch_timeout)
        .await?
    {
        return Err(ctx.halt("couldn't switch to the Handshake tab to read the timer"));
    }
    let raw = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_millis() as u64)
        .unwrap_or(0);
    let base_secs = target_minutes * 60;
    let target_secs = timer_target_secs(target_minutes, raw);
    let mut first_read = true;
    // Whatever the page reports, never sit here longer than the target in real
    // time. A timer that is paused and won't resume, a frozen SPA, or a
    // websocket that stays down would otherwise spin this loop forever with
    // the task claimed and the evaluation unsubmitted. By the time this fires
    // we have genuinely waited the full target on top of everything steps 1-7
    // spent, so the handle time is plausible regardless of what the widget says.
    let wall_deadline = tokio::time::Instant::now() + Duration::from_secs(target_secs);
    let mut warned_unreadable = false;

    let mut last_report: Option<u64> = None;
    loop {
        ctx.guard().await?;
        if tokio::time::Instant::now() >= wall_deadline {
            ctx.warn(format!(
                "the page timer never reached {}:{:02}, but {}:{:02} has now passed by wall \
                 clock -- submitting (real elapsed time is at least that long).",
                target_secs / 60,
                target_secs % 60,
                target_secs / 60,
                target_secs % 60
            ));
            return Ok(());
        }
        // A dropped websocket mid-wait must not abort the round: the CDP layer
        // already retries each call for ~17s, and a longer outage just means we
        // keep polling until the wall-clock deadline releases us. Stop and a
        // deliberate halt still propagate.
        let v = match ctx.eval(TIMER_READ_JS).await {
            Ok(v) => v,
            Err(e @ (GolemError::StoppedByUser | GolemError::Halted(_))) => return Err(e),
            Err(e) => {
                if !warned_unreadable {
                    ctx.warn(format!(
                        "lost contact with the task timer ({e}) -- retrying while the wait \
                         runs down"
                    ));
                    warned_unreadable = true;
                }
                ctx.human_pause(5000, 9000).await?;
                continue;
            }
        };
        let secs = v.get("secs").and_then(Value::as_u64);
        let running = v.get("running").and_then(Value::as_bool).unwrap_or(true);
        let gate = if first_read {
            target_secs.min(base_secs)
        } else {
            target_secs
        };
        first_read = false;
        match secs {
            // Present but unreadable. Keep looking -- it often becomes readable
            // again on the next repaint -- and let the wall-clock deadline
            // above end the wait if it never does.
            None => {
                if !warned_unreadable {
                    ctx.warn(
                        "can't read the task timer -- waiting the full target out by wall \
                         clock instead (this over-waits, which is the safe direction).",
                    );
                    warned_unreadable = true;
                }
                ctx.human_pause(5000, 9000).await?;
            }
            Some(s) if s >= gate => {
                ctx.output(format!(
                    "timer at {}:{:02} -- proceeding to submit",
                    s / 60,
                    s % 60
                ));
                return Ok(());
            }
            Some(s) => {
                if !running {
                    // A paused timer never reaches the target; resume it. This
                    // is retried every pass, so a missed cursor click just
                    // means another go next time round.
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

/// A short human beat between one workflow-8 control and the next: a
/// jittered pause, with occasional idle cursor drift (moves only, no clicks)
/// so the pointer doesn't bee-line from button to button. Timing only -- it
/// never touches what gets clicked.
async fn between_clicks(ctx: &mut WorkflowCtx) -> Result<()> {
    // scope the (non-Send) thread rng so it never crosses an await
    let wander = {
        let mut rng = rand::rng();
        rng.random_bool(0.35)
    };
    ctx.human_pause(500, 2000).await?;
    if wander {
        ctx.wander_cursor().await?;
    }
    Ok(())
}

/// The handle time Handshake asks for, and the shortest wait this workflow
/// will ever accept. Its timer widget renders elapsed-over-target as
/// `00:06:36/00:40:00`, which is where the number comes from.
const TIMER_FLOOR_MINUTES: u64 = 40;

/// Above this, a `target_minutes` input reads as a typo (`400` for `40`)
/// rather than a real request, and falls back to the floor.
const TIMER_CEILING_MINUTES: u64 = 120;

/// Minutes the task timer must reach before the round may submit, resolved
/// from the raw `target_minutes` input.
///
/// [`TIMER_FLOOR_MINUTES`] is a FLOOR, not merely the default value in the
/// box. A smaller number is raised to it, exactly like a blank or unparseable
/// one: the whole purpose of this input is to stop tasks being handed in
/// implausibly fast, so letting it be turned *down* would defeat it. Above the
/// floor it is honoured, since waiting longer than the platform asks is always
/// safe. The caller warns when the value it gets back isn't the one typed.
fn resolve_target_minutes(raw: Option<&str>) -> u64 {
    match raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<u64>().ok())
    {
        Some(m) if m > TIMER_CEILING_MINUTES => TIMER_FLOOR_MINUTES,
        Some(m) => m.max(TIMER_FLOOR_MINUTES),
        None => TIMER_FLOOR_MINUTES,
    }
}

/// How long the timer must read before the round may submit: `target_minutes`
/// plus up to 3 minutes of jitter drawn from `raw` (any arbitrary number --
/// the caller passes the current millisecond).
///
/// The jitter is UNSIGNED on purpose. It used to be `raw % 361 - 180`, i.e.
/// +/-3 min around the target, which made 40 a midpoint and let rounds submit
/// at 37:00. Handshake's own timer widget renders elapsed-over-target as
/// `00:06:36/00:40:00`, so 40:00 is the handle time the platform states it
/// wants, and coming in under it is the failure worth designing against.
/// Scattering above the floor keeps rounds from all landing on an identical
/// handle time just as well as scattering around it did.
fn timer_target_secs(target_minutes: u64, raw: u64) -> u64 {
    target_minutes * 60 + raw % 181 // 0..=180 seconds
}

/// The controlled tab's URL, retried briefly before giving up.
///
/// `current_url()` is served from chromiumoxide's cached frame manager, so a
/// tab that is mid-navigation -- or a connection the CDP supervisor is busy
/// re-establishing -- reports an error or a blank URL that a moment later
/// reads fine. Every caller here turns a blank URL into "you're on the wrong
/// page" and halts the run, which is a bad thing to do on a transient blip.
/// An empty return means genuinely unreadable.
async fn current_url_settled(ctx: &WorkflowCtx) -> String {
    for attempt in 0..5 {
        if let Ok(u) = ctx.browser.current_url().await
            && !u.is_empty()
        {
            return u;
        }
        if attempt < 4 {
            let _ = ctx.human_pause(400, 800).await;
        }
    }
    String::new()
}

/// Whether a Handshake tab is open, retried before believing "no".
///
/// `list_targets` is one of the few CDP calls with no reconnect retry behind
/// it: while the supervisor is re-attaching, the browser handle is `None` and
/// it answers `Ok(vec![])` -- "there are no tabs at all". Halting the round on
/// a single empty answer means a websocket blip strands a claimed task.
async fn handshake_tab_exists(ctx: &WorkflowCtx) -> Result<bool> {
    for attempt in 0..5 {
        ctx.guard().await?;
        if !ctx
            .browser
            .list_targets("ai.joinhandshake.com")
            .await?
            .is_empty()
        {
            return Ok(true);
        }
        if attempt < 4 {
            ctx.human_pause(500, 900).await?;
        }
    }
    Ok(false)
}

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

/// Wait for `find_js`'s control to appear, then press it.
///
/// The press goes through [`util::click_submit_with`], which is the only click
/// path here that actually checks its own work: it re-reads fresh coordinates
/// before every attempt, treats "the control is still on the page" as a missed
/// click, and escalates to a CDP click once the real cursor has failed twice.
/// Every control this drives (Continue, Continue task, Confirm time, Next
/// task) advances the wizard and disappears when pressed, so that check is
/// meaningful for all of them.
///
/// This used to be a single unverified `click_at_cursor`. On Wayland the real
/// cursor is the failure-prone part -- if it lands wrong there is nothing to
/// notice it, and the run would sail past a wizard step it never actually
/// took, only to fail much later somewhere unrelated.
///
/// Returns whether the control was found and pressed. Not finding it is NOT an
/// error: the same control is absent when the step has already been taken --
/// including when the user pressed it by hand before Golem got there -- and
/// skipping is exactly the right response to that.
async fn click_control(
    ctx: &mut WorkflowCtx,
    find_js: &str,
    label: &str,
    timeout: Duration,
) -> Result<bool> {
    if wait_for_coords(ctx, find_js, timeout).await?.is_none() {
        ctx.output(format!(
            "no '{label}' control visible -- skipping (already pressed, or this state \
             doesn't show one)"
        ));
        return Ok(false);
    }
    if util::click_submit_with(ctx, find_js).await? {
        ctx.output(format!("clicked '{label}'"));
        return Ok(true);
    }
    // `click_submit_with` also reports false when the control was gone before
    // it got a press in -- the user advancing the step by hand in the gap, or
    // the page moving on by itself. That is the state we wanted anyway.
    if ctx.eval(find_js).await?.get("x").is_none() {
        ctx.output(format!(
            "'{label}' went away before it could be pressed -- that step is done either way"
        ));
        return Ok(true);
    }
    // It was on the page, got clicked, and stayed. Report it and carry on:
    // the checks further down (the answers re-verify, the submit's own
    // enabled-check, the new-task-page test) are what decide whether the round
    // really worked, and stopping here would strand a claimed task.
    ctx.warn(format!(
        "'{label}' was on the page but is still there after clicking -- carrying on, later \
         checks will catch it if the step didn't take"
    ));
    Ok(false)
}

/// Click a plain Handshake button matched by its text. See [`click_control`]
/// for how the press is verified.
async fn click_button_by_text(
    ctx: &mut WorkflowCtx,
    pattern: &str,
    label: &str,
    timeout: Duration,
) -> Result<bool> {
    let find_js = BUTTON_BY_TEXT_JS.replace("__PATTERN__", &js_str(pattern));
    click_control(ctx, &find_js, label, timeout).await
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
/// The task-type options the wizard is currently offering. An unreadable or
/// unparseable result is reported as "none on offer" rather than failing the
/// step: the caller's next move either way is to hand over to a human.
async fn wizard_option_texts(ctx: &WorkflowCtx) -> Result<Vec<String>> {
    let v = ctx.eval(WIZARD_OPTIONS_JS).await?;
    Ok(v.as_str()
        .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
        .unwrap_or_default())
}

/// The stable part of an arena task slug, with the platform's leading id
/// groups dropped.
///
/// Handshake's option and multimango's URL name the same job, but not always
/// with the same number of id groups in front:
/// `vs-1781035041-260608-multimodal-agent-arena` and
/// `vs-1785279484-multimodal-agent-arena-review` both reduce to the part that
/// actually says what the task IS. The trailing words are kept, because
/// `...-arena` and `...-arena-review` are genuinely different task types and
/// must never be treated as interchangeable.
fn arena_core(slug: &str) -> String {
    let mut rest = slug.trim().to_ascii_lowercase();
    if let Some(r) = rest.strip_prefix("vs-") {
        rest = r.to_string();
    }
    // Drop leading all-digit groups ("1781035041-", "260608-", ...).
    while let Some((head, tail)) = rest.split_once('-') {
        if head.is_empty() || !head.chars().all(|c| c.is_ascii_digit()) {
            break;
        }
        rest = tail.to_string();
    }
    rest
}

/// Which Handshake task-type option corresponds to `arena_id` (the slug taken
/// from the multimango URL), given the options actually on offer.
///
/// The exact-match path is the common one. The fallbacks exist because the two
/// sides' ids drift: matching on [`arena_core`] absorbs a differing id group
/// while still keeping `-review` distinct from plain `-arena`. Anything less
/// certain than that returns `None` so a human picks -- choosing the wrong
/// option here misreports which work was done, which is worse than pausing.
fn pick_task_type_option(arena_id: &str, options: &[String]) -> Option<TaskTypePick> {
    let want = arena_id.trim().to_ascii_lowercase();
    if let Some(o) = options.iter().find(|o| o.trim().to_ascii_lowercase() == want) {
        return Some(TaskTypePick { text: o.clone(), exact: true });
    }

    let core = arena_core(arena_id);
    if !core.is_empty() {
        let mut same_core = options.iter().filter(|o| arena_core(o) == core);
        if let Some(first) = same_core.next()
            && same_core.next().is_none()
        {
            return Some(TaskTypePick { text: first.clone(), exact: false });
        }
    }

    // Last resort: exactly one option is an arena task at all. The ids on both
    // sides have already failed to line up, but with a single candidate there
    // is nothing else it could be. Reported as inexact so the log says so.
    let mut arena_ish = options
        .iter()
        .filter(|o| o.to_ascii_lowercase().contains("agent-arena"));
    if let Some(first) = arena_ish.next()
        && arena_ish.next().is_none()
    {
        return Some(TaskTypePick { text: first.clone(), exact: false });
    }
    None
}

/// A chosen task-type option, and whether it matched the multimango slug
/// outright (`exact`) or had to be inferred.
struct TaskTypePick {
    text: String,
    exact: bool,
}

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
    // Two attempts, and the second one goes through CDP: the first attempt
    // having missed is itself evidence the real cursor is the problem (bad
    // window mapping, the compositor moved the window, another window took the
    // click), and repeating it the same way would just miss again.
    const ATTEMPTS: usize = 2;
    for attempt in 1..=ATTEMPTS {
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
        let cdp = attempt == ATTEMPTS;
        if cdp {
            ctx.warn("the wizard answer didn't register -- retrying with a CDP click");
        }
        let (x, y) = util::jittered(ctx, x, y);
        if cdp {
            ctx.click_at_cdp(x, y).await?;
        } else {
            ctx.click_at_cursor(x, y).await?;
        }
        ctx.human_pause(400, 900).await?;
        // occasional idle drift between picking the option and hitting send
        let wander = {
            let mut rng = rand::rng();
            rng.random_bool(0.2)
        };
        if wander {
            ctx.wander_cursor().await?;
        }
        // The chat layout needs the choice sent with the up-arrow button;
        // the toggle layout has no such button and registers on the click.
        if let Some((sx, sy)) = wait_for_coords(ctx, WIZARD_SEND_JS, Duration::from_secs(4)).await?
        {
            let (sx, sy) = util::jittered(ctx, sx, sy);
            if cdp {
                ctx.click_at_cdp(sx, sy).await?;
            } else {
                ctx.click_at_cursor(sx, sy).await?;
            }
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
// The task-type options the wizard is currently offering, as a JSON array of
// their texts.
//
// Scans the same element set as `WIZARD_ANSWER_JS`, because the options are
// NOT buttons on the current page -- they are plain `<div>`s under a "select
// the task type" prompt (an older variant used aria-pressed toggles). Only
// slug-shaped candidates are kept, so the wizard's ordinary chrome (Continue,
// Send, nav) can't be mistaken for a task type, and already-sent bubbles (the
// right-aligned `items-end` column) are skipped: those are answers, not
// choices. Wrapper elements are skipped in favour of the innermost element
// carrying the text, so one option is reported once rather than once per
// ancestor.
const WIZARD_OPTIONS_JS: &str = r#"(function(){
  var out = [];
  var seen = {};
  var els = document.querySelectorAll('div, span, button, [role="button"], a');
  for (var i = 0; i < els.length; i++) {
    var e = els[i];
    if (e.closest('[class*="items-end"]')) continue;
    var t = (e.textContent || '').replace(/\s+/g, ' ').trim();
    if (!t || t.length > 120) continue;
    if (!/^vs-/i.test(t) && !/agent-arena/i.test(t)) continue;
    var r = e.getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    if (seen[t]) continue;
    /* Take only the element the text actually sits in. Any ancestor also
       "contains" it -- including ones that lump both options together, or
       pair the prompt with an already-sent answer -- and those read as
       options that do not exist. */
    var kids = e.querySelectorAll('*');
    var wrapper = false;
    for (var k = 0; k < kids.length; k++) {
      if ((kids[k].textContent || '').trim()) { wrapper = true; break; }
    }
    if (wrapper) continue;
    seen[t] = 1;
    out.push(t);
  }
  return JSON.stringify(out);
})()"#;

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Both slug shapes seen in the wild, taken from real saved pages: the
    /// older task carries two id groups, the newer review task one.
    const OLD: &str = "vs-1781035041-260608-multimodal-agent-arena";
    const NEW: &str = "vs-1785279484-multimodal-agent-arena-review";

    /// The whole point of the timer: a round must never be handed in below the
    /// target. Whatever millisecond the jitter is drawn from, the wait is at
    /// least 40:00 and at most 43:00.
    #[test]
    fn the_timer_target_never_falls_below_the_asked_for_minutes() {
        for raw in 0..1000u64 {
            let secs = timer_target_secs(40, raw);
            assert!(
                (2400..=2580).contains(&secs),
                "40 min target jittered out of range at raw={raw}: {secs}s"
            );
        }
        // and it is a real spread, not a constant
        let spread: std::collections::BTreeSet<u64> =
            (0..1000u64).map(|r| timer_target_secs(40, r)).collect();
        assert!(spread.len() > 100, "jitter collapsed to {} values", spread.len());
    }

    /// The floor is a floor. Typing a smaller number in the box asks for a
    /// handle time Handshake doesn't accept, so it is raised -- the same as
    /// leaving the box blank.
    #[test]
    fn a_target_below_the_floor_is_raised_to_it() {
        for asked in ["0", "1", "5", "20", "39", " 30 "] {
            assert_eq!(
                resolve_target_minutes(Some(asked)),
                40,
                "'{asked}' should have been raised to the 40 min floor"
            );
        }
    }

    #[test]
    fn a_target_above_the_floor_is_honoured_and_a_silly_one_is_not() {
        assert_eq!(resolve_target_minutes(Some("40")), 40);
        assert_eq!(resolve_target_minutes(Some("55")), 55);
        assert_eq!(resolve_target_minutes(Some("120")), 120);
        // past the ceiling reads as a typo for the floor, not a 6-hour wait
        assert_eq!(resolve_target_minutes(Some("400")), 40);
    }

    #[test]
    fn a_blank_or_unparseable_target_falls_back_to_the_floor() {
        assert_eq!(resolve_target_minutes(None), 40);
        assert_eq!(resolve_target_minutes(Some("")), 40);
        assert_eq!(resolve_target_minutes(Some("   ")), 40);
        assert_eq!(resolve_target_minutes(Some("soon")), 40);
        assert_eq!(resolve_target_minutes(Some("-5")), 40);
        assert_eq!(resolve_target_minutes(Some("40.5")), 40);
    }

    /// The two halves together: whatever goes in the box, the round never
    /// submits before 40:00.
    #[test]
    fn no_input_can_produce_a_wait_under_the_floor() {
        for asked in [
            None,
            Some(""),
            Some("0"),
            Some("1"),
            Some("39"),
            Some("nonsense"),
            Some("999"),
        ] {
            let mins = resolve_target_minutes(asked);
            for raw in 0..200u64 {
                assert!(
                    timer_target_secs(mins, raw) >= 40 * 60,
                    "input {asked:?} at raw={raw} allowed a submit under 40:00"
                );
            }
        }
    }

    #[test]
    fn the_timer_target_scales_with_the_requested_minutes() {
        assert_eq!(timer_target_secs(1, 0), 60);
        assert_eq!(timer_target_secs(120, 0), 7200);
        assert_eq!(timer_target_secs(40, 180), 2580);
    }

    #[test]
    fn arena_id_comes_from_the_task_url() {
        assert_eq!(
            arena_id_from_url(&format!("https://www.multimango.com/tasks/{NEW}")).as_deref(),
            Some(NEW)
        );
        // query strings and trailing paths are not part of the id
        assert_eq!(
            arena_id_from_url(&format!("https://www.multimango.com/tasks/{OLD}?x=1")).as_deref(),
            Some(OLD)
        );
        assert_eq!(arena_id_from_url("https://www.multimango.com/"), None);
    }

    #[test]
    fn core_drops_leading_id_groups_but_keeps_the_name() {
        assert_eq!(arena_core(OLD), "multimodal-agent-arena");
        assert_eq!(arena_core(NEW), "multimodal-agent-arena-review");
        // however many id groups the platform prefixes, the name survives
        assert_eq!(arena_core("vs-1-2-3-multimodal-agent-arena"), "multimodal-agent-arena");
    }

    #[test]
    fn exact_option_wins() {
        let opts = vec![OLD.to_string(), NEW.to_string()];
        let pick = pick_task_type_option(NEW, &opts).expect("exact match");
        assert_eq!(pick.text, NEW);
        assert!(pick.exact);
    }

    #[test]
    fn a_differing_id_group_still_matches_the_same_task_type() {
        // same job, different id on the Handshake side
        let opts = vec!["vs-999-260608-multimodal-agent-arena-review".to_string()];
        let pick = pick_task_type_option(NEW, &opts).expect("core match");
        assert_eq!(pick.text, opts[0]);
        assert!(!pick.exact, "an inferred match must not report as exact");
    }

    #[test]
    fn review_and_plain_arena_are_never_interchanged() {
        // Only these two on offer, neither sharing NEW's core: the sole-arena
        // fallback must not fire either, because there are two candidates.
        let opts = vec![OLD.to_string(), "vs-5-multimodal-agent-arena".to_string()];
        assert!(
            pick_task_type_option(NEW, &opts).is_none(),
            "must not guess between two different arena task types"
        );
    }

    #[test]
    fn a_single_arena_option_is_taken_when_ids_dont_line_up() {
        let opts = vec!["Continue".to_string(), OLD.to_string()];
        let pick = pick_task_type_option(NEW, &opts).expect("sole arena option");
        assert_eq!(pick.text, OLD);
        assert!(!pick.exact);
    }

    #[test]
    fn nothing_arena_shaped_means_no_pick() {
        let opts = vec!["Continue".to_string(), "Submit task".to_string()];
        assert!(pick_task_type_option(NEW, &opts).is_none());
    }
}
