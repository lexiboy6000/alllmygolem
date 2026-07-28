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
//! Cold start: if no multimango tab exists yet, the tab the button opens IS
//! the one to work, so it's kept rather than closed.
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

        // Snapshot the multimango tabs BEFORE clicking, so the tab the click
        // spawns can be told apart from the one we already drive by id.
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
        let _ = ctx.browser.bring_to_front().await;

        // ---- press the button, discard the duplicate --------------------
        ctx.step("click Open Multimango + close the extra tab").await?;
        match wait_for_coords(ctx, OPEN_MULTIMANGO_JS, Duration::from_secs(8)).await? {
            Some((x, y)) => {
                let (x, y) = util::jittered(ctx, x, y);
                ctx.click_at_cursor(x, y).await?;
                // Wait for the tab that appeared, then decide its fate: with a
                // pre-existing tab it's a duplicate and gets closed; with none,
                // it is the task tab and gets kept.
                let mut spawned: Option<String> = None;
                let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
                while tokio::time::Instant::now() < deadline {
                    ctx.guard().await?;
                    let now = ctx.browser.list_targets("multimango.com").await?;
                    if let Some(new_id) = now.iter().find(|id| !mm_before.contains(*id)) {
                        spawned = Some(new_id.clone());
                        break;
                    }
                    ctx.human_pause(400, 700).await?;
                }
                match spawned {
                    Some(id) if !mm_before.is_empty() => {
                        if ctx.browser.close_target_by_id(&id).await? {
                            ctx.output("closed the duplicate multimango tab");
                        } else {
                            ctx.warn(
                                "couldn't close the duplicate multimango tab -- continuing; \
                                 the switch below may land on the wrong one, so check the \
                                 task page before trusting the evaluation.",
                            );
                        }
                    }
                    Some(_) => {
                        // Cold start: nothing was open before, so the tab the
                        // button just opened is the task tab. Keep it.
                        ctx.output(
                            "no multimango tab was open before -- keeping the one Open \
                             Multimango just opened as the task tab",
                        );
                    }
                    None => {
                        ctx.warn(
                            "no new multimango tab appeared after clicking Open Multimango -- \
                             continuing (it may have opened elsewhere, or not at all)",
                        );
                    }
                }
            }
            None => {
                ctx.output(
                    "no 'Open Multimango' control visible -- skipping (already pressed, or \
                     this page state doesn't show one)",
                );
            }
        }

        // ---- hand control to the multimango task tab --------------------
        // Workflows 1-7 don't switch tabs; they drive whatever is controlled
        // here, so this switch is what makes the rest of the round work.
        ctx.step("return to the multimango task tab").await?;
        if !ctx
            .browser
            .switch_to_target("multimango.com", "", timeout)
            .await?
        {
            return Err(util::halt_unless_auto(
                ctx,
                "no multimango tab to switch to after the Open Multimango step -- workflows \
                 1-7 read the task from that tab, so open the arena task page and re-run.",
            )
            .await);
        }
        let _ = ctx.browser.bring_to_front().await;
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
