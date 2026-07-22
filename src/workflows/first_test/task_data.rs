//! Step 2: inside task1, create task_data/, download+unzip the "Download all
//! task data (ZIP)" link, and save the Task Data block's text as a file named
//! "information".

use crate::prelude::*;

use super::util;

pub struct SaveTaskData;

#[async_trait]
impl Workflow for SaveTaskData {
    fn name(&self) -> &'static str {
        "2. Save task data"
    }

    fn description(&self) -> &'static str {
        "Downloads+unzips the Task Data ZIP into task1/task_data and saves the Task Data text to task1/task_data/information."
    }

    fn dependencies(&self) -> Vec<&'static str> {
        vec!["1. Create task1 directory"]
    }

    fn inputs(&self) -> Vec<InputSpec> {
        vec![InputSpec::optional(
            "task_dir",
            "Task folder name (blank = same as step 1)",
            "",
        )]
    }

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        let data_dir = util::current_task_dir(ctx)?.join("task_data");

        ctx.step("create task_data directory").await?;
        std::fs::create_dir_all(&data_dir)
            .map_err(|e| GolemError::Io(format!("mkdir {}: {e}", data_dir.display())))?;

        ctx.step("download + unzip task data (if available)").await?;
        // Not every task has a downloadable ZIP -- some Task Data blocks are
        // just description text with no "Download all task data (ZIP)" link
        // at all. Wait (in case the page is still rendering) to see whether a
        // link genuinely exists; if it doesn't, skip the download entirely
        // instead of halting, and still fall through to save the text below.
        match util::wait_for_task_data_zip_url(ctx, Duration::from_secs(15)).await? {
            Some(url) => {
                ctx.output(format!("task data zip (informational): {url}"));
                // The actual download happens by clicking it for real, not by
                // fetching this URL directly -- that's 404'd inconsistently.
                let zip_path = util::click_and_wait_for_download(
                    ctx,
                    util::TASK_DATA_ZIP_CLICK_JS,
                    &data_dir,
                    "all_files.zip",
                    Duration::from_secs(30),
                )
                .await
                .map_err(|e| {
                    ctx.halt(format!(
                        "found a Task Data ZIP link but couldn't download it by clicking it: \
                         {e}. Make sure you're on a loaded, active task page (not the homepage \
                         or an expired task)."
                    ))
                })?;
                util::unzip_and_cleanup(ctx, &zip_path, &data_dir).await?;
                ctx.output(format!("unzipped task data into {}", data_dir.display()));
            }
            None => {
                ctx.output(
                    "no Task Data ZIP link found on this task -- skipping download (this task \
                     has no downloadable data, only the description text).",
                );
            }
        }

        ctx.step("save Task Data text").await?;
        let text = util::task_data_text(ctx).await?;
        if text.trim().is_empty() {
            ctx.warn("Task Data block text was empty");
        }
        let info_path = data_dir.join("information");
        std::fs::write(&info_path, text)
            .map_err(|e| GolemError::Io(format!("write {}: {e}", info_path.display())))?;
        ctx.output(format!("saved -> {}", info_path.display()));

        Ok(WorkflowOutcome::Completed)
    }
}
