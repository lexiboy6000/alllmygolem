//! "Navigate back to homepage" — click the "To do" tab, or fall back to the
//! home button, then verify we're on the feather homepage.

use crate::prelude::*;

use super::util;

pub struct NavigateHome;

#[async_trait]
impl Workflow for NavigateHome {
    fn name(&self) -> &'static str {
        "Navigate back to homepage"
    }
    fn description(&self) -> &'static str {
        "Return to the feather.openai.com homepage via 'To do' or the home button."
    }

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        ctx.step("return to homepage").await?;

        // feather can render the nav a beat after load — poll for the "To do"
        // link (preferred; matched by visible text) or the home button, and
        // click whichever appears first, up to the wait timeout.
        let timeout = Duration::from_millis(ctx.settings.default_wait_timeout_ms);
        let deadline = tokio::time::Instant::now() + timeout;
        let mut clicked = false;
        loop {
            ctx.guard().await?;
            if util::click_contains(ctx, "a", "To do").await? {
                clicked = true;
                break;
            }
            if ctx.exists("[data-testid=\"HomeIcon\"]").await? {
                ctx.click("[data-testid=\"HomeIcon\"]").await?;
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
                .stop_and_warn(
                    "Could not find the 'To do' link or the home button (page didn't load in time).",
                )
                .await);
        }

        ctx.step("verify on homepage").await?;
        if util::wait_until_on(ctx, "/", timeout).await? {
            ctx.output("back on the feather homepage");
            Ok(WorkflowOutcome::Completed)
        } else {
            let url = util::href(ctx).await.unwrap_or_default();
            Err(ctx
                .stop_and_warn(format!(
                    "expected the feather.openai.com homepage but URL is: {url}"
                ))
                .await)
        }
    }
}
