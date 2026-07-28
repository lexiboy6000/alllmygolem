//! Step 0: the Handshake busywork that has to happen BEFORE a task is worked.
//!
//! Handshake wants you to reach the arena through its own "Open Multimango"
//! button, but that button opens a brand-new multimango tab -- and the tab we
//! actually want to drive is the one already open (workflows 1-7 read the task
//! description, criteria and response zips out of it, and the ratings they
//! click live only in that tab's client-side state). So this workflow presses
//! the button to satisfy Handshake, immediately closes the duplicate tab it
//! spawns, and hands control back to the original multimango tab.
//!
//! Running FIRST is the point: workflows 1-7 never switch tabs themselves,
//! they just drive whatever tab is controlled. Workflow 1 depends on this one,
//! so every chain (including the next round queued by workflow 8) starts here.
//!
//! Handshake shows the control in two shapes and only one exists at a time: a
//! `<button>Open Multimango</button>` in the modal "Timer paused" dialog, or
//! the inline `<a target="_blank">Multimango</a>` links in the task's
//! "⚠️ Important" list (the normal state). Both open a tab pointing at
//! `multimango.com/sign-in?email={multimango_credentials}` -- an unsubstituted
//! template -- so the tab is a throwaway and gets closed again.
//!
//! The tab set is settled afterwards rather than by tracking one tab id:
//! prefer a tab already on `/tasks/<id>`, then close every OTHER multimango
//! tab. Cold start, the throwaway sign-in tab, and a previous round's leftover
//! all fall out of that one rule.
//!
//! Everything here is skip-safe -- no "Open Multimango" control visible (the
//! user already pressed it, or this state doesn't show one) means the step is
//! simply skipped, never a failure.

use crate::prelude::*;

use super::handshake_review::{OPEN_MULTIMANGO_JS, wait_for_coords};
use super::util;

pub struct OpenMultimango;

#[async_trait]
impl Workflow for OpenMultimango {
    fn name(&self) -> &'static str {
        "0. Open Multimango (Handshake busywork)"
    }

    fn description(&self) -> &'static str {
        "On the Handshake tab: clicks Open Multimango, closes the duplicate tab it opens, and \
         leaves the original multimango task tab controlled for workflows 1-7."
    }

    fn dependencies(&self) -> Vec<&'static str> {
        vec![]
    }

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        let timeout = Duration::from_millis(ctx.settings.default_wait_timeout_ms);

        ctx.step("note the multimango tabs already open").await?;
        let mm_before = ctx.browser.list_targets("multimango.com").await?;
        ctx.output(format!(
            "{} multimango tab(s) open before clicking",
            mm_before.len()
        ));

        // ---- switch to Handshake ----------------------------------------
        ctx.step("switch to the Handshake tab").await?;
        if !ctx
            .browser
            .switch_to_target("ai.joinhandshake.com", "", timeout)
            .await?
        {
            return Err(util::halt_unless_auto(
                ctx,
                "no Handshake tab to click 'Open Multimango' in -- open the task run page \
                 (ai.joinhandshake.com/.../task/<uuid>/run) in a tab next to multimango, \
                 then re-run.",
            )
            .await);
        }
        // MUST settle before the native click below -- see focus_and_settle.
        util::focus_and_settle(ctx).await?;

        // ---- press the button -------------------------------------------
        // It lives in Handshake's modal "Timer paused" dialog and is a plain
        // <button type="button"> with no href, so what it opens (a task tab, a
        // sign-in tab, or nothing at all) isn't knowable up front. Press it for
        // Handshake's benefit and let the tab cleanup below sort out the result
        // -- don't try to predict which tab appears, or when.
        // A NEW multimango tab appearing is the proof the press landed. That
        // works for both control shapes (the modal button and the inline
        // target="_blank" link), and unlike checking whether the control went
        // away it doesn't assume the click dismisses anything -- an anchor
        // stays right where it is. Without this check, a press swallowed by
        // the wrong tab is indistinguishable from success. Re-clicking is safe:
        // every surplus tab is closed by the cleanup below.
        ctx.step("click through to Multimango (opens a tab)").await?;
        let mut opened = false;
        for attempt in 0..3 {
            let find_timeout = if attempt == 0 { 8 } else { 3 };
            let Some((x, y)) =
                wait_for_coords(ctx, OPEN_MULTIMANGO_JS, Duration::from_secs(find_timeout)).await?
            else {
                ctx.output(
                    "no Multimango button or link visible on the Handshake page -- skipping",
                );
                break;
            };
            let (jx, jy) = util::jittered(ctx, x, y);
            ctx.click_at_cursor(jx, jy).await?;

            // Watch for the tab rather than guessing at a fixed sleep.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while tokio::time::Instant::now() < deadline {
                ctx.guard().await?;
                let now = ctx.browser.list_targets("multimango.com").await?;
                if now.iter().any(|id| !mm_before.contains(id)) {
                    opened = true;
                    break;
                }
                ctx.human_pause(400, 700).await?;
            }
            if opened {
                break;
            }
            ctx.warn(format!(
                "no new multimango tab after click {} -- the press probably landed on another \
                 tab; re-focusing and retrying",
                attempt + 1
            ));
            util::focus_and_settle(ctx).await?;
        }
        if opened {
            ctx.output("Multimango opened in a new tab -- it gets closed below");
        } else {
            ctx.warn(
                "never saw a new multimango tab open -- continuing to the tab cleanup anyway; \
                 click through to Multimango by hand if Handshake still expects it",
            );
        }

        // ---- settle on ONE multimango tab: the task page ----------------
        // Workflows 1-7 don't switch tabs; they drive whatever is controlled
        // here. Prefer a tab already on an arena task page, so a fresh sign-in
        // tab from the click can never become the one we work in.
        ctx.step("select the multimango task tab").await?;
        let on_task_page = ctx
            .browser
            .switch_to_target("multimango.com/tasks/", "", Duration::from_secs(8))
            .await
            .unwrap_or(false);
        if !on_task_page {
            ctx.warn(
                "no multimango tab is on an arena task page (.../tasks/<id>) -- falling back \
                 to any multimango tab",
            );
            if !ctx
                .browser
                .switch_to_target("multimango.com", "", timeout)
                .await?
            {
                return Err(util::halt_unless_auto(
                    ctx,
                    "no multimango tab at all after the Open Multimango step -- workflows \
                     1-7 read the task from that tab, so open the arena task page and re-run.",
                )
                .await);
            }
        }

        // Everything else multimango is surplus: the duplicate the button just
        // opened, a stale sign-in tab, a previous round's leftover. Closing by
        // "not the controlled one" rather than by id makes this independent of
        // whether (or how fast) a new tab actually appeared.
        ctx.step("close the extra multimango tabs").await?;
        match ctx.close_other_targets("multimango.com").await {
            Ok(0) => ctx.output("no extra multimango tabs to close"),
            Ok(n) => ctx.output(format!("closed {n} extra multimango tab(s)")),
            Err(e) => ctx.warn(format!(
                "couldn't close the extra multimango tabs ({e}) -- continuing with the task \
                 tab selected"
            )),
        }

        util::focus_and_settle(ctx).await?;
        let url = ctx.browser.current_url().await.unwrap_or_default();
        ctx.output(format!("controlling the multimango tab: {url}"));
        if !url.contains("multimango.com/tasks/") {
            ctx.warn(format!(
                "that tab isn't on an arena task page (expected multimango.com/tasks/<id>, \
                 got {url}) -- workflows 1-7 will read whatever is there."
            ));
        }

        Ok(WorkflowOutcome::Completed)
    }
}
