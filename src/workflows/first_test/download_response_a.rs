//! Step 4: download Response A's deliverables into task1/responseA -- the
//! all_files.zip on the older layout, or the individual files plus the
//! model-response text on the newer delivered-files listing (which has no
//! zip). See util::response_iframe_src for why this reads the iframe's src
//! instead of clicking the copy-link buttons (they live inside a
//! cross-origin iframe Golem's JS can't reach), and
//! util::download_response_into for how the two layouts are told apart.

use crate::prelude::*;

use super::util;

pub struct DownloadResponseA;

#[async_trait]
impl Workflow for DownloadResponseA {
    fn name(&self) -> &'static str {
        "4. Download Response A"
    }

    fn description(&self) -> &'static str {
        "Downloads Response A's deliverables (zip, or the newer per-file listing plus its model-response text) into task1/responseA."
    }

    fn dependencies(&self) -> Vec<&'static str> {
        vec!["3. Create responseA directory"]
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
            // ANY trouble getting Response A means this task can't be
            // evaluated: click the page's Skip button and restart the chain
            // at workflow 1 on the next task (a user Stop passes through).
            Err(e) => util::skip_and_restart(ctx, "Response A", e).await,
        }
    }
}

/// The download itself, separated from `run` so every failure -- iframe not
/// found, curl error, bad zip, unusable file listing -- funnels into the
/// skip-and-restart recovery.
async fn fetch(ctx: &mut WorkflowCtx) -> Result<()> {
    let dir = util::current_task_dir(ctx)?.join("responseA");

    ctx.step("find Response A's iframe").await?;
    let src = util::wait_for_response_iframe_src(ctx, "Response A", Duration::from_secs(15))
        .await?
        .ok_or_else(|| {
            ctx.halt(
                "couldn't find Response A's iframe (or its src wasn't a usable http(s) URL) \
                 after waiting 15s. Make sure you're on a loaded task page with Response A \
                 visible.",
            )
        })?;

    ctx.step("download Response A").await?;
    util::download_response_into(ctx, "Response A", &src, &dir).await
}
