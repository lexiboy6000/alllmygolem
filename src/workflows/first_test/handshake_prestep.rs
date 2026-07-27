//! Step 0: the pipeline's very first action, run BEFORE workflows 1-7 (it is
//! declared as the FIRST dependency of "8. Handshake review + submit", and
//! the resolver runs dependencies in declared order): on the Handshake task
//! page, click the "Open Multimango" control / Multimango link and
//! immediately close the tab it opens, then hand control back to the
//! multimango tab so workflows 1-7 can drive it. Records the Handshake run
//! URL in a marker file so workflow 8 skips its own step 1 for this round.

use crate::prelude::*;

use super::handshake_review::{open_multimango_and_close_tab, premm_marker_path};

pub struct HandshakePreOpenMultimango;

#[async_trait]
impl Workflow for HandshakePreOpenMultimango {
    fn name(&self) -> &'static str {
        "0. Handshake: open Multimango (pre-step)"
    }

    fn description(&self) -> &'static str {
        "Pipeline pre-step (runs before workflows 1-7): on the Handshake task page, click \
         Open Multimango / the Multimango link, immediately close the tab it opens, then \
         hand control back to the multimango tab."
    }

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        let timeout = Duration::from_millis(ctx.settings.default_wait_timeout_ms);

        // Fail fast, before 1-7 spend half an hour: the Handshake side must
        // be ready (task claimed, /run page open) for the round to complete.
        ctx.step("switch to the Handshake task page").await?;
        let hs_tabs = ctx.browser.list_targets("ai.joinhandshake.com").await?;
        if hs_tabs.is_empty() {
            return Err(ctx
                .stop_and_warn(
                    "no Handshake tab found -- open ai.joinhandshake.com's task page (the \
                     .../task/<uuid>/run URL) in a tab next to multimango, then re-run.",
                )
                .await);
        }
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
                     then re-run."
                ))
                .await);
        }

        ctx.step("click Open Multimango (closing the extra tab)").await?;
        let opened = open_multimango_and_close_tab(ctx).await?;

        // Only claim this round as handled if the dance ACTUALLY happened.
        // The helper is best-effort and returns Ok even when the control was
        // never on screen, so writing the marker unconditionally told
        // workflow 8 "the pre-step did it" -- step 1 then skipped as well and
        // the whole task ran without Open Multimango ever being clicked. That
        // is what broke the second task of a loop, where the freshly
        // navigated /run page had not rendered the control yet.
        let marker = premm_marker_path(ctx);
        if opened {
            // Best-effort: a failed write just means a harmless second
            // open+close later.
            if let Err(e) = std::fs::write(&marker, &hs_url) {
                ctx.warn(format!("couldn't write the pre-step marker ({e})"));
            }
        } else {
            // Clear any marker left by an earlier round so workflow 8's step
            // 1 retries the dance instead of skipping it.
            let _ = std::fs::remove_file(&marker);
            ctx.output(
                "Open Multimango wasn't clicked here -- leaving it to workflow 8's step 1",
            );
        }

        // Workflows 1-7 drive the multimango page -- hand control back.
        ctx.step("switch back to the multimango tab").await?;
        if !ctx
            .browser
            .switch_to_target("multimango.com", "", timeout)
            .await?
        {
            return Err(ctx.halt(
                "couldn't switch back to the multimango tab -- workflows 1-7 need it",
            ));
        }
        let _ = ctx.browser.bring_to_front().await;
        Ok(WorkflowOutcome::Completed)
    }
}
