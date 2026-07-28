//! Step 4: download Response A's deliverable zip and unzip it into
//! task1/responseA. See util::response_zip_url for why this derives the zip
//! URL from the iframe's src instead of clicking the "Copy link" button
//! (that button lives inside a cross-origin iframe Golem's JS can't reach).

use crate::prelude::*;

use super::util;

pub struct DownloadResponseA;

#[async_trait]
impl Workflow for DownloadResponseA {
    fn name(&self) -> &'static str {
        "4. Download Response A"
    }

    fn description(&self) -> &'static str {
        "Downloads and unzips Response A's deliverable files into task1/responseA."
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
/// found, curl error, bad zip -- funnels into the skip-and-restart recovery.
async fn fetch(ctx: &mut WorkflowCtx) -> Result<()> {
    let dir = util::current_task_dir(ctx)?.join("responseA");

    ctx.step("find + download Response A zip").await?;
    let zip_url = util::wait_for_response_zip_url(ctx, "Response A", Duration::from_secs(15))
        .await?
        .ok_or_else(|| {
            ctx.halt(
                "couldn't find Response A's iframe (or its src wasn't a usable http(s) URL) \
                 after waiting 15s. Make sure you're on a loaded task page with Response A \
                 visible.",
            )
        })?;
    ctx.output(format!("response A zip: {zip_url}"));
    let zip_path = util::download_into(ctx, &zip_url, &dir, "all_files.zip").await?;

    ctx.step("unzip Response A").await?;
    util::unzip_and_cleanup(ctx, &zip_path, &dir).await?;
    ctx.output(format!("unzipped Response A into {}", dir.display()));

    Ok(())
}
