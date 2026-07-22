//! "Claim task" — navigate Campaigns → Visual Demos v2 → the Ngspice task
//! batch, verifying the URL at each hop.
//!
//! The spec marks the steps *after* selecting the Ngspice batch as TODO, so this
//! workflow performs the specified navigation/verification and then surfaces a
//! clear notice that the remainder is unspecified.

use crate::prelude::*;

use super::util;

const CAMPAIGN_ID: &str = "00128237-6803-458d-8cc9-85829fc45321";
const BATCH_ID: &str = "80f1999d-b779-459b-9d33-987f87c48ba0";

pub struct ClaimTask;

#[async_trait]
impl Workflow for ClaimTask {
    fn name(&self) -> &'static str {
        "Claim task"
    }
    fn description(&self) -> &'static str {
        "Open Campaigns -> Visual Demos v2 -> Ngspice batch (claim steps are TODO in spec)."
    }
    fn dependencies(&self) -> Vec<&'static str> {
        vec!["Navigate and verify integrity"]
    }
    fn run_after(&self) -> Vec<&'static str> {
        vec!["Navigate back to homepage"]
    }

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        // feather is an SPA and can be slow: every element/URL check below POLLS
        // up to the configured wait timeout instead of checking once, so a slow
        // render no longer trips a "could not find …" error.
        let timeout = Duration::from_millis(ctx.settings.default_wait_timeout_ms);

        // --- Campaigns ---
        ctx.step("open Campaigns").await?;
        if ctx.wait_for("a[href=\"/campaigns\"]", timeout).await.is_err() {
            return Err(ctx
                .stop_and_warn("Could not find the 'Campaigns' link (page didn't load in time).")
                .await);
        }
        ctx.click("a[href=\"/campaigns\"]").await?;
        if !util::wait_until_on(ctx, "/campaigns", timeout).await? {
            let url = util::href(ctx).await.unwrap_or_default();
            return Err(ctx
                .stop_and_warn(format!("expected feather.openai.com/campaigns, at: {url}"))
                .await);
        }

        // --- Visual Demos v2 ---
        ctx.step("open Visual Demos v2").await?;
        let campaign_sel = format!("a[href=\"/campaigns/{CAMPAIGN_ID}\"]");
        if ctx.wait_for(&campaign_sel, timeout).await.is_err() {
            return Err(ctx
                .stop_and_warn(
                    "Could not find the 'Visual Demos v2' campaign link (page didn't load in time).",
                )
                .await);
        }
        ctx.click(&campaign_sel).await?;
        if !util::wait_until_on(ctx, &format!("/campaigns/{CAMPAIGN_ID}"), timeout).await? {
            let url = util::href(ctx).await.unwrap_or_default();
            return Err(ctx
                .stop_and_warn(format!("expected the Visual Demos v2 campaign page, at: {url}"))
                .await);
        }

        // --- Ngspice task batch ---
        // Poll for the batch (by aria-label or visible text) and click it once it
        // renders; the batch list often appears a beat after the campaign loads.
        ctx.step("select the Ngspice task batch").await?;
        let deadline = tokio::time::Instant::now() + timeout;
        let mut clicked = false;
        loop {
            ctx.guard().await?;
            if ctx.exists("[aria-label=\"Ngspice\"]").await? {
                ctx.click("[aria-label=\"Ngspice\"]").await?;
                clicked = true;
                break;
            }
            if util::click_contains(ctx, "span", "Ngspice").await? {
                clicked = true;
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            ctx.human_pause(200, 400).await?;
        }
        if !clicked {
            return Err(ctx
                .stop_and_warn("Could not find the 'Ngspice' task batch (page didn't load in time).")
                .await);
        }

        // The batch selection navigates asynchronously — poll the URL.
        if !util::wait_until_href_contains(ctx, &format!("task-batch={BATCH_ID}"), timeout).await? {
            let url = util::href(ctx).await?;
            return Err(ctx
                .stop_and_warn(format!(
                    "expected the Ngspice batch (task-batch={BATCH_ID}) in the URL, at: {url}"
                ))
                .await);
        }
        let url = util::href(ctx).await?;
        ctx.output("Ngspice batch selected (unclaimed tasks view).");

        // Spec marks the actual claim steps as TODO.
        ctx.warn_user(
            "The remainder of 'Claim task' (selecting and claiming an individual task) is \
             not yet specified in the workflow doc (marked TODO). Stopping after selecting \
             the Ngspice batch.",
        )
        .await?;

        Ok(WorkflowOutcome::CompletedWith(json!({
            "campaign_id": CAMPAIGN_ID,
            "batch_id": BATCH_ID,
            "url": url,
            "note": "claim steps unspecified (TODO in spec)",
        })))
    }
}
