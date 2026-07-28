//! Step 2: inside task1, create task_data/, download+unzip the task data
//! (either the "Download all task data (ZIP)" link or the newer archive
//! viewer's "Download" button), and save the Task Data block's text as a file
//! named "information".

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
        // Not every task has downloadable data -- some Task Data blocks are
        // just description text with no download link/button at all. Wait (in
        // case the page is still rendering) to see whether one genuinely
        // exists; if it doesn't, skip the download entirely instead of
        // halting, and still fall through to save the text below.
        match util::wait_for_task_data_download(ctx, Duration::from_secs(15)).await? {
            Some(util::TaskDataSource::DirectUrl(url)) => {
                ctx.output(format!("task data zip: {url}"));
                // Fetch the href directly with a bare curl first -- the same
                // mechanism the response downloads use. An earlier direct-fetch
                // attempt "404'd inconsistently", but that went through
                // ctx.download, which attaches the page's multimango cookies;
                // this host 404s on foreign cookies (see util::download_into),
                // while a bare curl gets a clean 200. Clicking the link is kept
                // only as a fallback: on the newer task layout (responses shown
                // as "Download archive" file-listing pages) the physical click
                // stopped producing a file in ~/Downloads at all.
                let zip_path = match util::download_into(ctx, &url, &data_dir, "all_files.zip")
                    .await
                {
                    Ok(path) => path,
                    Err(e) => {
                        ctx.warn(format!(
                            "direct download of the Task Data ZIP failed ({e}); falling back \
                             to clicking the link"
                        ));
                        util::click_and_wait_for_download(
                            ctx,
                            util::TASK_DATA_ZIP_CLICK_JS,
                            &data_dir,
                            "all_files.zip",
                            Duration::from_secs(90),
                        )
                        .await
                        .map_err(|e| {
                            ctx.halt(format!(
                                "found a Task Data ZIP link but couldn't download it directly \
                                 or by clicking it: {e}. Make sure you're on a loaded, active \
                                 task page (not the homepage or an expired task)."
                            ))
                        })?
                    }
                };
                util::unzip_and_cleanup(ctx, &zip_path, &data_dir).await?;
                ctx.output(format!("unzipped task data into {}", data_dir.display()));
            }
            Some(util::TaskDataSource::DownloadButton) => {
                // Newer layout: the Task Data block shows an archive viewer
                // ("Input Data Files", a file tree) whose Download control is
                // a <button> with no href anywhere in the DOM -- nothing to
                // curl, so clicking it is the only way to get the file.
                ctx.output("task data is behind a Download button (no direct link) -- clicking it");
                let zip_path = util::click_and_wait_for_download(
                    ctx,
                    util::TASK_DATA_ZIP_CLICK_JS,
                    &data_dir,
                    "all_files.zip",
                    Duration::from_secs(90),
                )
                .await
                .map_err(|e| {
                    ctx.halt(format!(
                        "found the Task Data Download button but clicking it produced no \
                         file: {e}. Make sure you're on a loaded, active task page (not the \
                         homepage or an expired task)."
                    ))
                })?;
                // The button is expected to serve a ZIP of the listed files,
                // but nothing in the DOM guarantees that -- sniff the magic
                // bytes and keep a non-ZIP as-is rather than halting on unzip.
                let is_zip = std::fs::read(&zip_path)
                    .map(|b| b.starts_with(b"PK"))
                    .unwrap_or(false);
                if is_zip {
                    util::unzip_and_cleanup(ctx, &zip_path, &data_dir).await?;
                    ctx.output(format!("unzipped task data into {}", data_dir.display()));
                } else {
                    ctx.warn(format!(
                        "downloaded file doesn't look like a ZIP -- keeping it as-is at {}",
                        zip_path.display()
                    ));
                }
            }
            None => {
                ctx.output(
                    "no Task Data download link or button found on this task -- skipping \
                     download (this task has no downloadable data, only the description text).",
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
