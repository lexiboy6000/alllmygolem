//! Step 5: same as step 4, but for Response B -- including clicking over to
//! its tab on the newest arena layout. task1/responseB is created by
//! util::capture_response (no separate directory-creation step was needed).

use crate::prelude::*;

use super::util;

pub struct DownloadResponseB;

#[async_trait]
impl Workflow for DownloadResponseB {
    fn name(&self) -> &'static str {
        "5. Download Response B"
    }

    fn description(&self) -> &'static str {
        "Saves Response B into task1/responseB: the inline response page when the task embeds it, otherwise its zip or per-file listing plus the model-response text."
    }

    fn dependencies(&self) -> Vec<&'static str> {
        vec!["4. Download Response A"]
    }

    fn inputs(&self) -> Vec<InputSpec> {
        vec![InputSpec::optional(
            "task_dir",
            "Task folder name (blank = same as step 1)",
            "",
        )]
    }

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        match fetch(ctx).await {
            Ok(()) => Ok(WorkflowOutcome::Completed),
            // See the same spot in `download_response_a.rs`: a 4xx means the
            // deliverable isn't there and the task is unwinnable, so skip it
            // and restart; anything else is a transient failure worth
            // surfacing rather than throwing a good task away.
            Err(e) if util::is_missing_file_error(&e) => {
                util::skip_and_restart(ctx, "Response B", e).await
            }
            Err(e) => Err(e),
        }
    }
}

/// The download itself, separated from `run` so every failure -- iframe not
/// found, curl error, bad zip, unusable file listing -- funnels into the
/// skip-and-restart recovery.
async fn fetch(ctx: &mut WorkflowCtx) -> Result<()> {
    let dir = util::current_task_dir(ctx)?.join("responseB");

    ctx.step("open the Response B tab").await?;
    util::activate_response_tab(ctx, "Response B").await?;

    // capture_response creates responseB itself, so no separate mkdir step.
    ctx.step("capture Response B").await?;
    util::capture_response(ctx, "Response B", &dir).await
}
