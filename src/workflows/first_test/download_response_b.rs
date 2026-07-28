//! Step 5: same as step 4, but for Response B. Also creates task1/responseB
//! itself (no separate directory-creation step was needed for it).

use crate::prelude::*;

use super::util;

pub struct DownloadResponseB;

#[async_trait]
impl Workflow for DownloadResponseB {
    fn name(&self) -> &'static str {
        "5. Download Response B"
    }

    fn description(&self) -> &'static str {
        "Downloads and unzips Response B's deliverable files into task1/responseB."
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
            // ANY trouble getting Response B means this task can't be
            // evaluated: click the page's Skip button and restart the chain
            // at workflow 1 on the next task (a user Stop passes through).
            Err(e) => util::skip_and_restart(ctx, "Response B", e).await,
        }
    }
}

/// The download itself, separated from `run` so every failure -- iframe not
/// found, curl error, bad zip -- funnels into the skip-and-restart recovery.
async fn fetch(ctx: &mut WorkflowCtx) -> Result<()> {
    let dir = util::current_task_dir(ctx)?.join("responseB");

    ctx.step("create responseB directory").await?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| GolemError::Io(format!("mkdir {}: {e}", dir.display())))?;

    ctx.step("find + download Response B zip").await?;
    let zip_url = util::wait_for_response_zip_url(ctx, "Response B", Duration::from_secs(15))
        .await?
        .ok_or_else(|| {
            ctx.halt(
                "couldn't find Response B's iframe (or its src wasn't a usable http(s) URL) \
                 after waiting 15s. Make sure you're on a loaded task page with Response B \
                 visible.",
            )
        })?;
    ctx.output(format!("response B zip: {zip_url}"));
    let zip_path = util::download_into(ctx, &zip_url, &dir, "all_files.zip").await?;

    ctx.step("unzip Response B").await?;
    util::unzip_and_cleanup(ctx, &zip_path, &dir).await?;
    ctx.output(format!("unzipped Response B into {}", dir.display()));

    Ok(())
}
