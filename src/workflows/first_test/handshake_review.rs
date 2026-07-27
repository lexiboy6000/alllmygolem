//! Step 8: the fully-automatic submission leg of the full task pipeline
//! (Pause/Resume to intervene).
//!
//! Runs after steps 1-7 (its dependency chain) have downloaded the task,
//! judged it with Claude, and clicked the answers into the multimango page --
//! with the chain's `defer_submit` input set, step 7 leaves Submit untouched.
//!
//! The Handshake side is a chat-style stepper (answers are picked, then sent
//! with an up-arrow send button). The exact flow, per the user's walkthrough
//! (2026-07-27):
//!
//! 1. click the Open Multimango / "Multimango" link and immediately close the
//!    new tab it opens (we already drive one, WITH the applied ratings) --
//!    in the pipeline this runs FIRST, before workflows 1-7, via the
//!    "0. Handshake: open Multimango (pre-step)" dependency (declared ahead
//!    of workflow 7 in the dependency list); its marker file makes this
//!    step a skip here,
//! 2. click "Continue" on the pre-task instructions screen (the controls
//!    after it don't exist in the DOM until then),
//! 3. select the arena task-type button (id derived from the multimango tab's
//!    URL), then click the up-arrow send button,
//! 4. click the "Continue task" popup that follows,
//! 5. wait out the handle time: the full `target_minutes` input +/- up to
//!    ~3 min of jitter, counted from when this step starts (plain wall
//!    clock, NOT the page timer),
//! 6. re-verify the answers on multimango, then on Handshake select
//!    "I submitted my task on Multimango" and click the up-arrow send,
//! 7. submit the evaluation on the multimango tab,
//! 8. click the "Submit task" button on Handshake,
//! 9. back on the Handshake tab, click the confirmation "Submit task" that
//!    follows -- the ACTUAL submission only happens on this second click,
//! 10. click "Confirm time", then "Next task", and -- once Handshake is on a
//!     new task run page -- queue the next round, looping until `rounds`
//!     tasks (default 10) are completed in this one run.
//!
//! MANUAL PROGRESS IS SKIPPED: before step 1 the Handshake page is probed
//! (one eval per finder). The chat is a linear stepper, so a later-stage
//! element on screen proves every earlier step was already done by hand --
//! those steps are skipped instead of re-run. A hand-submitted multimango
//! evaluation is also handled: if the evaluation buttons are entirely gone
//! at the re-verify, step 7 is skipped with a loud warning.
//!
//! TESTING JUST THIS LEG (steps 1-7 already done: answers applied on the
//! multimango page, `claude_answers` on disk): hit Run on this workflow and
//! pick "Skip prerequisites" in the prompt -- the chain runner then runs only
//! this workflow. A blank `task_dir` resolves to the task folder recorded by
//! the last step-1 run (the `.golem_current_task` marker file).
//!
//! FULLY AUTOMATIC -- no permission prompts (user request): the whole leg
//! runs end to end without asking. The intervention mechanism is the
//! engine's Pause/Resume (overlay or main window), honored before every
//! step, wait tick and click via `ctx.guard()`; Stop aborts the run and
//! discards the queued next round. Remaining safeguards:
//! - workflow 7 never touches multimango's Submit when `defer_submit` is set
//!   (the pipeline always sets it) -- the submit happens here, at step 7;
//! - the answers are re-verified against the live page after the wait,
//!   before anything is submitted or told to Handshake (the platform can
//!   swap the open task under us); a hand-submitted evaluation (no
//!   evaluation buttons at all) is skipped with a loud warning.

use crate::prelude::*;

use super::util;

pub struct HandshakeReviewAndSubmit;

#[async_trait]
impl Workflow for HandshakeReviewAndSubmit {
    fn name(&self) -> &'static str {
        "8. Handshake review + submit (pipeline)"
    }

    fn description(&self) -> &'static str {
        "Full pipeline leg, fully automatic (use Pause/Resume to intervene): Handshake \
         chat flow, timed multimango + Handshake submission, then loops through the next \
         tasks until `rounds` (default 10) are done."
    }

    fn dependencies(&self) -> Vec<&'static str> {
        vec![
            // Declared FIRST so it runs first: the open-Multimango dance
            // happens on Handshake BEFORE any of the 1-7 multimango work.
            "0. Handshake: open Multimango (pre-step)",
            "7. Answer + apply evaluation criteria",
        ]
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
                "Minutes the wait step holds before submitting, +/- ~3 min jitter",
                "35",
            ),
            InputSpec::optional(
                "rounds",
                "Tasks to complete in this one run (loops via Next task; 1 = no loop)",
                "10",
            ),
        ]
    }

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        let timeout = Duration::from_millis(ctx.settings.default_wait_timeout_ms);
        let task_dir = util::current_task_dir(ctx)?;

        let target_minutes: u64 = ctx
            .input("target_minutes")
            .and_then(|v| v.trim().parse().ok())
            .filter(|m| (1..=120).contains(m))
            .unwrap_or(35);

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
            return Err(ctx
                .stop_and_warn(
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
        let _ = ctx.browser.bring_to_front().await;
        let hs_url = ctx.browser.current_url().await.unwrap_or_default();
        if !(hs_url.contains("/task/") && hs_url.contains("/run")) {
            return Err(ctx
                .stop_and_warn(format!(
                    "the Handshake tab is at {hs_url}, not a task run page \
                     (.../annotations/fellow/task/<uuid>/run). Claim/open the task there, \
                     then re-run this workflow."
                ))
                .await);
        }

        // ---- how far did the human already get? --------------------------
        // One instant eval per finder, no polling: the chat is a linear
        // stepper, so a later-stage element on screen proves every earlier
        // step is already done by hand -- those get skipped, not re-run.
        ctx.step("probe how far the Handshake chat already is").await?;
        let select_js = ARENA_SELECT_JS.replace("__ARENA__", &js_str(&arena_id));
        let stage = probe_stage(ctx, &select_js).await?;
        if stage > HsStage::Start {
            ctx.output(format!("manual progress detected: {}", stage.describe()));
        }

        if stage < HsStage::TaskType {

        // ---- step 1: open Multimango, immediately close the new tab ------
        // In the pipeline this normally already happened BEFORE workflows
        // 1-7, via "0. Handshake: open Multimango (pre-step)" -- the marker
        // it writes (keyed on this exact run URL) makes this a skip.
        let premm_done = std::fs::read_to_string(premm_marker_path(ctx))
            .map(|s| s.trim() == hs_url)
            .unwrap_or(false);
        if premm_done {
            ctx.output(
                "Open Multimango already handled by the pipeline pre-step -- skipping",
            );
        } else {
            ctx.step("click Open Multimango (closing the extra tab)").await?;
            open_multimango_and_close_tab(ctx).await?;
        }

        // ---- step 2: dismiss the pre-task instructions screen ------------
        // The /run page opens on an "⚠️ Important" instructions screen
        // (task steps + notes) whose bottom-right "Continue" button gates
        // everything after it: the task-type controls are NOT in the DOM
        // until it's clicked (verified against a saved copy of the screen).
        // Skips cleanly when the screen isn't there / was dismissed by hand.
        ctx.step("click Continue on the instructions screen").await?;
        match wait_for_coords(ctx, CONTINUE_GATE_JS, Duration::from_secs(6)).await? {
            Some((x, y)) => {
                let (x, y) = util::jittered(ctx, x, y);
                ctx.click_at_cursor(x, y).await?;
                ctx.output("clicked Continue on the instructions screen");
                ctx.human_pause(800, 1500).await?;
            }
            None => {
                ctx.output("no instructions 'Continue' gate visible -- skipping");
            }
        }

        } else {
            ctx.output(
                "instructions screen already dismissed by hand -- skipping Open \
                 Multimango + Continue",
            );
        }

        if stage < HsStage::ContinueTask {

        // ---- step 3: select the arena task type, then send it ------------
        ctx.step("select the task type on Handshake").await?;
        // The task-type controls render only after the instructions screen
        // is dismissed -- wait for them to appear, and if they don't, retry
        // the Continue gate once (its click may have missed).
        if wait_for_coords(ctx, &select_js, Duration::from_secs(12))
            .await?
            .is_none()
            && let Some((x, y)) = wait_for_coords(ctx, CONTINUE_GATE_JS, Duration::from_secs(3)).await?
        {
            ctx.output("task-type buttons not visible yet -- clicking Continue again");
            let (x, y) = util::jittered(ctx, x, y);
            ctx.click_at_cursor(x, y).await?;
            let _ = wait_for_coords(ctx, &select_js, Duration::from_secs(12)).await?;
        }
        if !util::click_until_selected(ctx, &select_js).await? {
            dump_buttons(ctx).await;
            return Err(ctx
                .stop_and_warn(format!(
                    "couldn't find/select the '{arena_id}' task-type button on the \
                     Handshake page (the visible buttons were just logged). Select it \
                     by hand and re-run, or fix the finder."
                ))
                .await);
        }
        ctx.output(format!("selected task type: {arena_id}"));

        // Send the picked task type with the chat composer's up-arrow
        // button; the "Continue task" popup is what tells us it landed.
        ctx.step("send the task-type answer (up-arrow)").await?;
        match send_and_expect(ctx, CONTINUE_TASK_JS, "Continue task").await? {
            Some((x, y)) => {
                // ---- step 4: the "Continue task" popup -------------------
                ctx.step("click the Continue task popup").await?;
                let (x, y) = util::jittered(ctx, x, y);
                ctx.click_at_cursor(x, y).await?;
                ctx.output("clicked Continue task");
            }
            None => {
                ctx.warn_user(
                    "After selecting the task type, the up-arrow send / 'Continue task' \
                     popup didn't behave as expected. Do those two clicks by hand (send \
                     the answer, then Continue task), then dismiss this message.",
                )
                .await?;
            }
        }

        } else if stage == HsStage::ContinueTask {
            // The human already picked + sent the task type; only the popup
            // is left to click.
            ctx.step("click the Continue task popup").await?;
            match wait_for_coords(ctx, CONTINUE_TASK_JS, Duration::from_secs(8)).await? {
                Some((x, y)) => {
                    let (x, y) = util::jittered(ctx, x, y);
                    ctx.click_at_cursor(x, y).await?;
                    ctx.output("clicked Continue task");
                }
                None => {
                    ctx.warn_user(
                        "the 'Continue task' popup disappeared -- click it by hand if \
                         it's still needed, then dismiss this message.",
                    )
                    .await?;
                }
            }
        } else {
            ctx.output("task type + Continue task already handled by hand -- skipping");
        }

        if stage == HsStage::SubmitTask {
            ctx.output(
                "the chat is already at Submit task -- skipping the handle-time wait \
                 (the manual clicks imply the human owns the timing)",
            );
        } else {

        // ---- step 5: wait out the handle time ---------------------------
        // Plain wall clock (NOT the page's timer widget): the full
        // target_minutes input +/- up to 3 min of jitter, counted from the
        // moment this step starts.
        ctx.step("wait out the handle time before submitting").await?;
        wait_handle_time(ctx, target_minutes).await?;

        }

        // ---- re-verify the answers on multimango ------------------------
        // Safeguard, BEFORE telling Handshake anything was submitted: the
        // platform can swap the open task while we were waiting -- submitting
        // then would rate a DIFFERENT task. Verify every approved answer is
        // still selected; re-apply once if not.
        ctx.step("re-verify the answers on multimango").await?;
        if !ctx
            .browser
            .switch_to_target("multimango.com", "", timeout)
            .await?
        {
            return Err(ctx.halt("couldn't switch back to the multimango tab"));
        }
        let _ = ctx.browser.bring_to_front().await;
        let answers = util::read_claude_answers(&task_dir.join("claude_answers"))?;
        let mut mm_submitted_by_hand = false;
        let mut wrong = util::verify_answers_applied(ctx, &answers).await?;
        if !wrong.is_empty() {
            ctx.warn(format!(
                "{} answer(s) not selected on the page -- re-applying once",
                wrong.len()
            ));
            let (applied, _missed) = util::apply_answers(ctx, &answers).await?;
            wrong = util::verify_answers_applied(ctx, &answers).await?;
            if !wrong.is_empty() && applied == 0 {
                // Re-apply couldn't click a single button: the evaluation UI
                // isn't on the page at all. In this flow that means the human
                // already submitted it by hand (they sometimes do) -- skip
                // the multimango submit. No permission prompt (user request);
                // if the page is instead in some broken state, this loud
                // warning plus Pause/Stop are the intervention points.
                mm_submitted_by_hand = true;
                wrong.clear();
                ctx.warn(
                    "no evaluation buttons on the multimango page at all -- assuming the \
                     evaluation was already submitted by hand; skipping the multimango \
                     submit (Pause or Stop now if that's wrong)",
                );
            }
            if !wrong.is_empty() {
                return Err(ctx
                    .stop_and_warn(format!(
                        "even after re-applying, these answers aren't selected: {} -- the \
                         open multimango task probably CHANGED since they were applied. \
                         NOT submitting; check the page.",
                        wrong.join(", ")
                    ))
                    .await);
            }
        }

        // ---- step 6: tell Handshake the task is submitted ---------------
        if stage == HsStage::SubmitTask {
            ctx.output(
                "'I submitted my task on Multimango' already sent by hand ('Submit task' \
                 is on screen) -- skipping step 6",
            );
        } else {

        ctx.step("select 'I submitted my task on Multimango' + send").await?;
        if !ctx
            .browser
            .switch_to_target("ai.joinhandshake.com", "", timeout)
            .await?
        {
            return Err(ctx.halt("couldn't switch back to the Handshake tab"));
        }
        let _ = ctx.browser.bring_to_front().await;
        match wait_for_coords(ctx, I_SUBMITTED_JS, Duration::from_secs(15)).await? {
            Some((x, y)) => {
                let (x, y) = util::jittered(ctx, x, y);
                ctx.click_at_cursor(x, y).await?;
                ctx.human_pause(400, 900).await?;
            }
            None => {
                ctx.warn_user(
                    "couldn't find the 'I submitted my task on Multimango' option on the \
                     Handshake page. Select it by hand (do NOT send it yet), then dismiss \
                     this message.",
                )
                .await?;
            }
        }
        // The "Submit task" button appearing is what tells us the send landed.
        if send_and_expect(ctx, SUBMIT_TASK_JS, "Submit task").await?.is_none() {
            ctx.warn_user(
                "the 'Submit task' button hasn't appeared after sending 'I submitted my \
                 task on Multimango'. Bring the Handshake page to that point by hand \
                 (don't click Submit task itself), then dismiss this message.",
            )
            .await?;
        }

        }

        // ---- step 7: submit the evaluation on multimango ----------------
        if !mm_submitted_by_hand {
            ctx.step("submit the evaluation on multimango").await?;
            if !ctx
                .browser
                .switch_to_target("multimango.com", "", timeout)
                .await?
            {
                return Err(ctx.halt("couldn't switch back to the multimango tab"));
            }
            let _ = ctx.browser.bring_to_front().await;
            if !util::click_submit_if_enabled(ctx).await? {
                return Err(ctx
                    .stop_and_warn(
                        "the multimango Submit button wasn't found or never enabled -- \
                         submit by hand, then continue on the Handshake side manually.",
                    )
                    .await);
            }
            ctx.output("multimango evaluation submitted");
        }

        // ---- step 8: Submit task on Handshake ---------------------------
        // No permission prompt here (user request): the leg runs end to end
        // automatically. Pause/Resume (overlay or main window) is the
        // intervention point, honored before every step and click.
        ctx.step("click Submit task on Handshake").await?;
        if !ctx
            .browser
            .switch_to_target("ai.joinhandshake.com", "", timeout)
            .await?
        {
            return Err(ctx.halt("couldn't switch back to the Handshake tab"));
        }
        let _ = ctx.browser.bring_to_front().await;
        if !util::click_submit_with(ctx, SUBMIT_TASK_JS).await? {
            return Err(ctx
                .stop_and_warn(
                    "the Handshake 'Submit task' button wasn't found or didn't register -- \
                     submit it by hand.",
                )
                .await);
        }
        ctx.output("Submit task clicked");

        // ---- step 9: confirm Submit task (the ACTUAL submission) --------
        // Step 8's click brings up a confirmation with a second "Submit
        // task" button; the task is NOT submitted until that one is clicked.
        // Re-switch to the Handshake tab first in case the first click moved
        // focus. If no confirmation shows within 15s, step 8's click (or its
        // click_submit_with retries) already went all the way through.
        ctx.step("confirm Submit task (final submission)").await?;
        let _ = ctx
            .browser
            .switch_to_target("ai.joinhandshake.com", "", timeout)
            .await;
        let _ = ctx.browser.bring_to_front().await;
        match wait_for_coords(ctx, SUBMIT_TASK_JS, Duration::from_secs(15)).await? {
            Some(_) => {
                if util::click_submit_with(ctx, SUBMIT_TASK_JS).await? {
                    ctx.output("confirmation Submit task clicked -- task submitted");
                } else {
                    ctx.warn_user(
                        "the confirmation 'Submit task' button wouldn't click away -- \
                         click it by hand, then dismiss this message.",
                    )
                    .await?;
                }
            }
            None => {
                ctx.output(
                    "no confirmation 'Submit task' appeared -- the submission already \
                     went through on the first click",
                );
            }
        }
        ctx.output("Handshake task submitted");

        // ---- step 10: Confirm time --------------------------------------
        // After the submission goes through, Handshake asks to confirm the
        // recorded handle time.
        ctx.step("click Confirm time").await?;
        match wait_for_coords(ctx, CONFIRM_TIME_JS, Duration::from_secs(20)).await? {
            Some(_) => {
                if util::click_submit_with(ctx, CONFIRM_TIME_JS).await? {
                    ctx.output("Confirm time clicked");
                } else {
                    ctx.warn_user(
                        "the 'Confirm time' button wouldn't click away -- click it by \
                         hand, then dismiss this message.",
                    )
                    .await?;
                }
            }
            None => {
                ctx.output("no 'Confirm time' button appeared within 20s -- continuing");
            }
        }

        // ---- step 11: Next task + queue the next round ------------------
        ctx.step("click Next task").await?;
        let prev_run_url = hs_url;
        match wait_for_coords(ctx, CONTINUE_TASK_JS, Duration::from_secs(30)).await? {
            Some((x, y)) => {
                let (x, y) = util::jittered(ctx, x, y);
                ctx.click_at_cursor(x, y).await?;
            }
            None => {
                ctx.warn_user(
                    "couldn't find a 'Next task' (or 'Continue task') button after \
                     submitting. Click it yourself (or claim the next task), then \
                     dismiss this message.",
                )
                .await?;
            }
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

        // `rounds` counts the TOTAL tasks this one run should complete
        // (default 10); each queued round carries rounds-1, and the loop
        // ends when it hits 1.
        let rounds: u32 = ctx
            .input("rounds")
            .and_then(|v| v.trim().parse().ok())
            .filter(|r| (1..=100).contains(r))
            .unwrap_or(10);
        if on_new_task && rounds > 1 {
            let mut inputs = std::collections::BTreeMap::new();
            // task_dir stays empty so step 1 auto-creates the next taskN.
            inputs.insert("defer_submit".to_string(), "yes".to_string());
            inputs.insert(
                "target_minutes".to_string(),
                target_minutes.to_string(),
            );
            inputs.insert("rounds".to_string(), (rounds - 1).to_string());
            ctx.queue_chain(vec![self.name().to_string()], inputs);
            ctx.output(format!(
                "next task queued -- {} more to go after this chain finishes (press \
                 Stop to end the loop early)",
                rounds - 1
            ));
        } else if !on_new_task {
            ctx.warn(
                "Handshake didn't move to a new task run page -- not queueing another \
                 round. Start the next pipeline manually when ready.",
            );
        } else {
            ctx.output("all tasks for this run are done -- not queueing another round.");
        }

        Ok(WorkflowOutcome::Completed)
    }
}

// ---------------------------------------------------------------------------
// open Multimango + close the tab (shared with the pipeline pre-step)
// ---------------------------------------------------------------------------

/// Click the control that opens Multimango in a NEW tab and immediately
/// close that tab (we already drive a multimango tab -- the one WITH the
/// applied ratings, which live only in its client-side state). Entirely
/// best-effort: the human sometimes handles this by hand -- every outcome
/// just notes what happened and moves on; this never fails the workflow.
/// Used by workflow 8's step 1 and by "0. Handshake: open Multimango".
pub(super) async fn open_multimango_and_close_tab(ctx: &mut WorkflowCtx) -> Result<()> {
    let mm_before = ctx
        .browser
        .list_targets("multimango.com")
        .await
        .unwrap_or_default();
    match wait_for_coords(ctx, OPEN_MULTIMANGO_JS, Duration::from_secs(8)).await? {
        Some((x, y)) => {
            let (x, y) = util::jittered(ctx, x, y);
            ctx.click_at_cursor(x, y).await?;
            // Close exactly the tab that appeared, never the original. If
            // the human closes it first, that's just as good.
            let mut seen: Option<String> = None;
            let mut note = "no new multimango tab appeared -- nothing to close \
                            (it may have been closed by hand or not opened at all)";
            let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
            while tokio::time::Instant::now() < deadline {
                ctx.guard().await?;
                let Ok(now) = ctx.browser.list_targets("multimango.com").await else {
                    note = "couldn't list tabs while looking for the extra multimango \
                            tab -- leaving it be";
                    break;
                };
                match &seen {
                    None => {
                        if let Some(new_id) = now.iter().find(|id| !mm_before.contains(*id)) {
                            seen = Some(new_id.clone());
                            if ctx.browser.close_target_by_id(new_id).await.unwrap_or(false) {
                                note = "closed the extra multimango tab";
                                break;
                            }
                            // Close missed (tab already gone?) -- the next
                            // poll sees whether it still exists.
                        }
                    }
                    Some(id) => {
                        if !now.contains(id) {
                            note = "the extra multimango tab was already closed \
                                    (by hand?) -- skipping";
                            break;
                        }
                        if ctx.browser.close_target_by_id(id).await.unwrap_or(false) {
                            note = "closed the extra multimango tab";
                            break;
                        }
                    }
                }
                ctx.human_pause(400, 700).await?;
            }
            ctx.output(note);
            // Handshake may have yielded focus to the popup; come back.
            let _ = ctx.browser.bring_to_front().await;
        }
        None => {
            ctx.output(
                "no 'Open Multimango' control visible -- skipping (it only shows in \
                 some dialogs/states, or was already handled by hand)",
            );
        }
    }
    Ok(())
}

/// Marker recording the Handshake run URL for which the pipeline pre-step
/// already did the open-Multimango dance -- lets workflow 8 skip its own
/// step 1 in the same round. Self-invalidates when the run URL changes
/// (every round gets a fresh /task/<uuid>/run).
pub(super) fn premm_marker_path(ctx: &WorkflowCtx) -> std::path::PathBuf {
    ctx.settings.output_dir.join(".golem_hs_premm")
}

// ---------------------------------------------------------------------------
// manual-progress probe
// ---------------------------------------------------------------------------

/// How far the human has already advanced the Handshake chat by hand.
/// Ordered: a later variant proves every earlier step is complete (the chat
/// is a linear stepper), so everything before it gets skipped.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum HsStage {
    /// No later-stage element on screen: run the flow from step 1.
    Start,
    /// Task-type buttons on screen: instructions already dismissed.
    TaskType,
    /// "Continue task" popup on screen: task type already sent.
    ContinueTask,
    /// "I submitted my task on Multimango" on screen: past Continue task.
    ISubmitted,
    /// "Submit task" on screen: step 6 already done by hand.
    SubmitTask,
}

impl HsStage {
    fn describe(self) -> &'static str {
        match self {
            HsStage::Start => "none",
            HsStage::TaskType => {
                "instructions already dismissed (task-type buttons on screen)"
            }
            HsStage::ContinueTask => "task type already sent ('Continue task' on screen)",
            HsStage::ISubmitted => {
                "already past Continue task ('I submitted my task...' on screen)"
            }
            HsStage::SubmitTask => "already at the final step ('Submit task' on screen)",
        }
    }
}

/// One instant eval per finder, latest stage first. The instructions screen
/// is checked FIRST: its bare "Continue" gate button would otherwise trip
/// CONTINUE_TASK_JS's bare-"continue" fallback, which is also why the probe
/// uses the STRICT continue-task matcher.
async fn probe_stage(ctx: &mut WorkflowCtx, select_js: &str) -> Result<HsStage> {
    if found(ctx, CONTINUE_GATE_JS).await? {
        return Ok(HsStage::Start);
    }
    if found(ctx, SUBMIT_TASK_JS).await? {
        return Ok(HsStage::SubmitTask);
    }
    if found(ctx, I_SUBMITTED_JS).await? {
        return Ok(HsStage::ISubmitted);
    }
    if found(ctx, CONTINUE_TASK_STRICT_JS).await? {
        return Ok(HsStage::ContinueTask);
    }
    if found(ctx, select_js).await? {
        return Ok(HsStage::TaskType);
    }
    Ok(HsStage::Start)
}

/// Whether `find_js` locates its element right now (single eval, no polling).
async fn found(ctx: &mut WorkflowCtx, find_js: &str) -> Result<bool> {
    let v = ctx.eval(find_js).await?;
    Ok(v.get("x").and_then(Value::as_f64).is_some())
}

// ---------------------------------------------------------------------------
// the handle-time wait + chat-composer send
// ---------------------------------------------------------------------------

/// Wait out the full handle time from RIGHT NOW: `target_minutes` +/- up to
/// 3 min of jitter (so rounds don't all land on the exact same handle time).
/// Plain wall clock -- the page's timer widget is not read. The project
/// expects handle times near the human average; the wait only exists so
/// tasks aren't turned around implausibly fast. Stop/Pause-aware; progress
/// is reported about once a minute.
async fn wait_handle_time(ctx: &mut WorkflowCtx, target_minutes: u64) -> Result<()> {
    let jitter_secs: i64 = {
        use rand::RngExt;
        rand::rng().random_range(-180..=180)
    };
    let total_secs = (target_minutes as i64 * 60 + jitter_secs).max(60) as u64;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(total_secs);
    ctx.output(format!(
        "waiting {}:{:02} from now ({} min +/- up to 3 min jitter) before submitting",
        total_secs / 60,
        total_secs % 60,
        target_minutes
    ));
    let mut last_report: Option<u64> = None;
    loop {
        ctx.guard().await?;
        let now = tokio::time::Instant::now();
        if now >= deadline {
            ctx.output("handle time reached -- proceeding to submit");
            return Ok(());
        }
        let left = (deadline - now).as_secs();
        if last_report.is_none_or(|r| r.saturating_sub(left) >= 60) {
            ctx.output(format!("{}:{:02} left before submitting", left / 60, left % 60));
            last_report = Some(left);
        }
        ctx.human_pause(5000, 9000).await?;
    }
}

/// Click the chat composer's up-arrow send button, then wait for
/// `expect_desc` (found by `expect_js`) -- the element the flow should show
/// next -- and return its coordinates. The send button can legitimately stay
/// on screen for the next chat message, so "still visible" is NOT proof the
/// click missed: only the missing next element is, and that gets exactly one
/// more send attempt before giving up (never a blind rapid re-fire).
async fn send_and_expect(
    ctx: &mut WorkflowCtx,
    expect_js: &str,
    expect_desc: &str,
) -> Result<Option<(f64, f64)>> {
    for attempt in 1..=2 {
        match wait_for_coords(ctx, UPARROW_SEND_JS, Duration::from_secs(10)).await? {
            Some((x, y)) => {
                let (x, y) = util::jittered(ctx, x, y);
                ctx.click_at_cursor(x, y).await?;
            }
            None => {
                if attempt == 1 {
                    ctx.output(
                        "no enabled up-arrow send button visible -- maybe already sent; \
                         watching for what should come next",
                    );
                }
            }
        }
        if let Some(coords) = wait_for_coords(ctx, expect_js, Duration::from_secs(20)).await? {
            return Ok(Some(coords));
        }
        if attempt == 1 {
            ctx.output(format!(
                "'{expect_desc}' hasn't appeared -- trying the send once more"
            ));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// small helpers + page finders
// ---------------------------------------------------------------------------

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

/// Poll `find_js` (an IIFE returning `{x, y}` or null) until it yields
/// coordinates or `timeout` passes.
async fn wait_for_coords(
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

/// Log the visible buttons on the page -- called when a finder misses so the
/// mismatch is debuggable from the output pane alone.
async fn dump_buttons(ctx: &mut WorkflowCtx) {
    if let Ok(v) = ctx.eval(DUMP_BUTTONS_JS).await
        && let Some(s) = v.as_str()
    {
        ctx.output(format!("visible buttons on the page: {s}"));
    }
}

/// The control that opens Multimango in a NEW tab: an exact "Open Multimango"
/// button/link (task guide / return-reminder dialogs), else the instructions
/// screen's "Multimango" link (verified in the saved screen: `<a
/// href="https://www.multimango.com/sign-in..." target="_blank">Multimango</a>`).
/// Only `target="_blank"` anchors are eligible for the fallback -- a same-tab
/// link would navigate the Handshake tab away.
const OPEN_MULTIMANGO_JS: &str = r#"(function(){
  var els = document.querySelectorAll('button, a, [role="button"]');
  for (var i = 0; i < els.length; i++) {
    if ((els[i].textContent || '').trim().toLowerCase() !== 'open multimango') continue;
    try { els[i].scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
    var r = els[i].getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
  }
  var links = document.querySelectorAll('a[href*="multimango.com"][target="_blank"]');
  for (var j = 0; j < links.length; j++) {
    try { links[j].scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
    var r2 = links[j].getBoundingClientRect();
    if (r2.width < 1 || r2.height < 1) continue;
    return { x: r2.left + r2.width / 2, y: r2.top + r2.height / 2 };
  }
  return null;
})()"#;

/// The pre-task instructions screen's gating "Continue" button (bottom
/// right; exact text/aria-label "Continue", verified in the saved screen).
/// Nothing after it -- task-type buttons included -- exists in the DOM until
/// it's clicked. Deliberately does NOT match "Continue task" (post-submit).
const CONTINUE_GATE_JS: &str = r#"(function(){
  var els = document.querySelectorAll('button, [role="button"]');
  for (var i = 0; i < els.length; i++) {
    var b = els[i];
    var txt = (b.textContent || '').trim().toLowerCase();
    var al = (b.getAttribute('aria-label') || '').trim().toLowerCase();
    if (txt !== 'continue' && al !== 'continue') continue;
    if (b.disabled || b.getAttribute('aria-disabled') === 'true') continue;
    try { b.scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
    var r = b.getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
  }
  return null;
})()"#;

/// The task-type control whose exact text is the arena id. Selection state
/// is `aria-pressed` (verified in the saved page: toggle buttons under
/// "Please select the task type you're working on"); `aria-selected` covers
/// the same control rendered as a dropdown option ("certified task
/// dropdown" in the instructions screen's wording).
const ARENA_SELECT_JS: &str = r#"(function(){
  var WANT = __ARENA__;
  var btns = document.querySelectorAll('button, [role="option"], [role="button"]');
  for (var i = 0; i < btns.length; i++) {
    if ((btns[i].textContent || '').trim() !== WANT) continue;
    try { btns[i].scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
    var r = btns[i].getBoundingClientRect();
    if (r.width < 1 || r.height < 1) return null;
    return { x: r.left + r.width / 2, y: r.top + r.height / 2,
             selected: btns[i].getAttribute('aria-pressed') === 'true'
                    || btns[i].getAttribute('aria-selected') === 'true' };
  }
  return null;
})()"#;

/// The chat composer's up-arrow send button, enabled only: a button
/// labelled/reading exactly "Submit" (Handshake labels the send that way), or
/// an icon-only button carrying an arrow-up SVG. Deliberately never matches
/// "Submit task" (that's SUBMIT_TASK_JS) -- the exact-text check excludes it.
const UPARROW_SEND_JS: &str = r#"(function(){
  var btns = document.querySelectorAll('button');
  for (var i = 0; i < btns.length; i++) {
    var b = btns[i];
    if (b.disabled || b.getAttribute('aria-disabled') === 'true') continue;
    var al = (b.getAttribute('aria-label') || '').trim().toLowerCase();
    var txt = (b.textContent || '').trim().toLowerCase();
    var arrow = b.querySelector('svg[class*="arrow-up"], svg [class*="arrow-up"]');
    var isSend = (al === 'submit' || txt === 'submit') || (arrow && txt === '');
    if (!isSend) continue;
    try { b.scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
    var r = b.getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
  }
  return null;
})()"#;

/// The "I submitted my task on Multimango" answer option in the chat flow
/// (step 6). Restricted to interactive elements whose text STARTS with the
/// phrase, so an already-sent chat bubble containing the same words in a
/// plain div never matches.
const I_SUBMITTED_JS: &str = r#"(function(){
  var els = document.querySelectorAll('button, [role="button"], [role="option"], [role="radio"], label');
  for (var i = 0; i < els.length; i++) {
    var b = els[i];
    var t = (b.textContent || '').trim().toLowerCase();
    if (t.indexOf('i submitted my task') !== 0) continue;
    if (b.disabled || b.getAttribute('aria-disabled') === 'true') continue;
    try { b.scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
    var r = b.getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
  }
  return null;
})()"#;

/// The final "Submit task" button on Handshake (step 8; also the element
/// whose appearance confirms step 6's send landed). Exact text/aria-label,
/// enabled + visible only.
const SUBMIT_TASK_JS: &str = r#"(function(){
  var els = document.querySelectorAll('button, [role="button"]');
  for (var i = 0; i < els.length; i++) {
    var b = els[i];
    var txt = (b.textContent || '').trim().toLowerCase();
    var al = (b.getAttribute('aria-label') || '').trim().toLowerCase();
    if (txt !== 'submit task' && al !== 'submit task') continue;
    if (b.disabled || b.getAttribute('aria-disabled') === 'true') continue;
    try { b.scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
    var r = b.getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
  }
  return null;
})()"#;

/// The "Confirm time" button Handshake shows after the submission goes
/// through (confirms the recorded handle time). Exact text/aria-label,
/// enabled + visible only.
const CONFIRM_TIME_JS: &str = r#"(function(){
  var els = document.querySelectorAll('button, [role="button"]');
  for (var i = 0; i < els.length; i++) {
    var b = els[i];
    var txt = (b.textContent || '').trim().toLowerCase();
    var al = (b.getAttribute('aria-label') || '').trim().toLowerCase();
    if (txt !== 'confirm time' && al !== 'confirm time') continue;
    if (b.disabled || b.getAttribute('aria-disabled') === 'true') continue;
    try { b.scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
    var r = b.getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
  }
  return null;
})()"#;

/// Exact-text "Continue task" only, no bare-"continue" fallback -- used by
/// the stage probe, where the fallback could misfire on unrelated buttons.
const CONTINUE_TASK_STRICT_JS: &str = r#"(function(){
  var els = document.querySelectorAll('button, a, [role="button"]');
  for (var i = 0; i < els.length; i++) {
    var t = (els[i].textContent || '').trim().toLowerCase();
    if (t !== 'continue task' && t !== 'continue to task') continue;
    if (els[i].disabled || els[i].getAttribute('aria-disabled') === 'true') continue;
    var r = els[i].getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
  }
  return null;
})()"#;

/// The move-on button: post-submit "Next task" scores highest, then the
/// step-4 "Continue task" popup, then a bare enabled "Continue" as fallback.
/// (Also used by step 3's send-verify, where only "Continue task" exists.)
const CONTINUE_TASK_JS: &str = r#"(function(){
  var els = document.querySelectorAll('button, a, [role="button"]');
  var best = null, bestScore = 0;
  for (var i = 0; i < els.length; i++) {
    var t = (els[i].textContent || '').trim().toLowerCase();
    var score = (t === 'next task') ? 3
              : (t === 'continue task' || t === 'continue to task') ? 2
              : (t === 'continue' ? 1 : 0);
    if (!score || score <= bestScore) continue;
    if (els[i].disabled || els[i].getAttribute('aria-disabled') === 'true') continue;
    best = els[i]; bestScore = score;
  }
  if (!best) return null;
  try { best.scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
  var r = best.getBoundingClientRect();
  if (r.width < 1 || r.height < 1) return null;
  return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
})()"#;

/// Every visible button's/option's text, for diagnostics when a finder misses.
const DUMP_BUTTONS_JS: &str = r#"(function(){
  var btns = document.querySelectorAll('button, [role="button"], [role="option"], [role="combobox"], select');
  var out = [];
  for (var i = 0; i < btns.length && out.length < 40; i++) {
    var r = btns[i].getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    var t = (btns[i].textContent || '').trim().slice(0, 50);
    if (t) out.push(t);
  }
  return out.join(' | ');
})()"#;
