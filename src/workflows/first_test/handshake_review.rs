//! Step 8: the human-supervised submission leg of the full task pipeline.
//!
//! Runs after steps 1-7 (its dependency chain) have downloaded the task,
//! judged it with Claude, and clicked the answers into the multimango page --
//! with the chain's `defer_submit` input set, step 7 leaves Submit untouched.
//! This workflow then:
//!
//! 1. switches to the Handshake task tab (`ai.joinhandshake.com/.../task/<uuid>/run`),
//! 2. clicks "Open Multimango" if it's shown (Handshake busywork; the extra
//!    multimango tab it opens is closed again -- we already drive one),
//! 3. selects the arena task type (derived from the multimango tab's URL),
//! 4. raises the "GOLEM NEEDS YOU" prompt and BLOCKS until the human reviewer
//!    either signs off, sends feedback to Claude (re-judge + re-apply, then
//!    ask again), or aborts,
//! 5. only after sign-off: waits for the Handshake task timer to reach the
//!    expected handle time (~35-40 min), re-verifies the answers are still on
//!    the page, submits on multimango, then submits on Handshake,
//! 6. clicks "Continue task" and queues the next pipeline round.
//!
//! SAFEGUARDS -- nothing can submit before the human sign-off:
//! - step 7 never touches Submit when `defer_submit` is set (the pipeline
//!   always sets it);
//! - every submit call in this file lives AFTER the review loop returns
//!   `Approved`; feedback and abort paths loop or bail out before them;
//! - the answers are re-verified against the live page right before the
//!   multimango submit (the platform can swap the open task under us);
//! - Stop discards the queued next round, so stopping ends the whole loop.

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

        // ---- Handshake busywork: Open Multimango ------------------------
        ctx.step("click Open Multimango (closing the extra tab)").await?;
        let mm_before = ctx.browser.list_targets("multimango.com").await?;
        match wait_for_coords(ctx, OPEN_MULTIMANGO_JS, Duration::from_secs(8)).await? {
            Some((x, y)) => {
                let (x, y) = util::jittered(ctx, x, y);
                ctx.click_at_cursor(x, y).await?;
                // The click opens a fresh multimango tab we don't need (we
                // already drive one, WITH the applied ratings -- which live
                // only in that tab's client-side state). Close exactly the
                // tab that appeared, never the original.
                let mut closed = false;
                let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
                while tokio::time::Instant::now() < deadline {
                    ctx.guard().await?;
                    let now = ctx.browser.list_targets("multimango.com").await?;
                    if let Some(new_id) = now.iter().find(|id| !mm_before.contains(*id)) {
                        closed = ctx.browser.close_target_by_id(new_id).await?;
                        break;
                    }
                    ctx.human_pause(400, 700).await?;
                }
                if closed {
                    ctx.output("closed the extra multimango tab");
                } else {
                    ctx.warn(
                        "no new multimango tab appeared after clicking Open Multimango -- \
                         continuing (it may have opened elsewhere or not at all)",
                    );
                }
                // Handshake may have yielded focus to the popup; come back.
                let _ = ctx.browser.bring_to_front().await;
            }
            None => {
                ctx.output(
                    "no 'Open Multimango' control visible -- skipping (it only shows in \
                     some dialogs/states)",
                );
            }
        }

        // ---- select the arena task type ---------------------------------
        ctx.step("select the task type on Handshake").await?;
        let select_js = ARENA_SELECT_JS.replace("__ARENA__", &js_str(&arena_id));
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
        let _ = ctx.browser.bring_to_front().await;
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
            let _ = util::apply_answers(ctx, &answers).await?;
            wrong = util::verify_answers_applied(ctx, &answers).await?;
        }
        if !wrong.is_empty() {
            return Err(ctx
                .stop_and_warn(format!(
                    "even after re-applying, these answers aren't selected: {} -- the open \
                     multimango task probably CHANGED since the review. NOT submitting; \
                     check the page.",
                    wrong.join(", ")
                ))
                .await);
        }
        if !util::click_submit_if_enabled(ctx).await? {
            return Err(ctx
                .stop_and_warn(
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
        let _ = ctx.browser.bring_to_front().await;
        if !util::click_submit_with(ctx, HANDSHAKE_SUBMIT_JS).await? {
            return Err(ctx
                .stop_and_warn(
                    "the Handshake Submit button wasn't found or never enabled (is the \
                     task-type still selected?). Submit by hand.",
                )
                .await);
        }
        ctx.output("Handshake task submitted");

        // ---- continue to the next task + queue the next round -----------
        ctx.step("continue to the next task").await?;
        let prev_run_url = hs_url;
        match wait_for_coords(ctx, CONTINUE_TASK_JS, Duration::from_secs(30)).await? {
            Some((x, y)) => {
                let (x, y) = util::jittered(ctx, x, y);
                ctx.click_at_cursor(x, y).await?;
            }
            None => {
                ctx.warn_user(
                    "couldn't find a 'Continue task' button after submitting. Click it \
                     yourself (or claim the next task), then dismiss this message.",
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

        let loop_pipeline = ctx
            .input("loop_pipeline")
            .is_some_and(|v| !v.trim().is_empty());
        if on_new_task && loop_pipeline {
            let mut inputs = std::collections::BTreeMap::new();
            // task_dir stays empty so step 1 auto-creates the next taskN.
            inputs.insert("defer_submit".to_string(), "yes".to_string());
            inputs.insert(
                "target_minutes".to_string(),
                target_minutes.to_string(),
            );
            inputs.insert("loop_pipeline".to_string(), "yes".to_string());
            ctx.queue_chain(vec![self.name().to_string()], inputs);
            ctx.output(
                "next pipeline round queued -- it starts as soon as this chain finishes \
                 (press Stop to end the loop).",
            );
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
                        let (applied, missed) = util::apply_answers(ctx, &answers).await?;
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

/// A visible "Open Multimango" button/link (shows in Handshake's task guide /
/// return-reminder dialogs).
const OPEN_MULTIMANGO_JS: &str = r#"(function(){
  var els = document.querySelectorAll('button, a, [role="button"]');
  for (var i = 0; i < els.length; i++) {
    if ((els[i].textContent || '').trim().toLowerCase() !== 'open multimango') continue;
    try { els[i].scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
    var r = els[i].getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
  }
  return null;
})()"#;

/// The task-type button whose exact text is the arena id. Selection state is
/// `aria-pressed` (verified in the saved page: toggle buttons under
/// "Please select the task type you're working on").
const ARENA_SELECT_JS: &str = r#"(function(){
  var WANT = __ARENA__;
  var btns = document.querySelectorAll('button');
  for (var i = 0; i < btns.length; i++) {
    if ((btns[i].textContent || '').trim() !== WANT) continue;
    try { btns[i].scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
    var r = btns[i].getBoundingClientRect();
    if (r.width < 1 || r.height < 1) return null;
    return { x: r.left + r.width / 2, y: r.top + r.height / 2,
             selected: btns[i].getAttribute('aria-pressed') === 'true' };
  }
  return null;
})()"#;

/// Handshake's enabled submit: `button[type=submit]` labelled/reading
/// "Submit" that isn't disabled (it stays disabled until a task type is
/// selected).
const HANDSHAKE_SUBMIT_JS: &str = r#"(function(){
  var btns = document.querySelectorAll('button[type="submit"]');
  for (var i = 0; i < btns.length; i++) {
    var b = btns[i];
    var al = (b.getAttribute('aria-label') || '').trim().toLowerCase();
    var txt = (b.textContent || '').trim().toLowerCase();
    if (al !== 'submit' && txt !== 'submit') continue;
    if (b.disabled || b.getAttribute('aria-disabled') === 'true') continue;
    try { b.scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
    var r = b.getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
  }
  return null;
})()"#;

/// A "Continue task"-style button after submitting (exact matches first, a
/// bare enabled "Continue" as fallback).
const CONTINUE_TASK_JS: &str = r#"(function(){
  var els = document.querySelectorAll('button, a, [role="button"]');
  var best = null, bestScore = 0;
  for (var i = 0; i < els.length; i++) {
    var t = (els[i].textContent || '').trim().toLowerCase();
    var score = (t === 'continue task' || t === 'continue to task') ? 2
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
