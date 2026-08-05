//! Step 4: capture Response A into task1/responseA. What that means depends on
//! how the page carries the response (util::capture_response tells them apart):
//!
//! - newest arena layout: the response is INLINE, written into its iframe's
//!   `srcdoc` attribute. Nothing is downloaded -- there is no per-response
//!   host, no zip and no file listing -- the document is read straight off the
//!   parent's DOM and saved as response.html.
//! - hosted layouts: the iframe's `src` points at the response's own host, and
//!   util::download_response_into sorts out the all_files.zip, the
//!   delivered-files listing (individual files plus the model-response text),
//!   or a plain rendered page.
//!
//! See util::response_srcdoc / util::response_iframe_src for why both are read
//! as attributes rather than by reaching into the iframe: on the hosted layout
//! it is cross-origin, and on the inline layout it is sandboxed without
//! allow-same-origin, so its document is unreachable either way.
//!
//! On the newest layout the two responses share one pane behind a tab strip,
//! so the Response A tab is clicked first -- that is how a person brings it on
//! screen, and it makes sure the iframe has actually rendered.

use crate::prelude::*;

use super::util;

pub struct DownloadResponseA;

#[async_trait]
impl Workflow for DownloadResponseA {
    fn name(&self) -> &'static str {
        "4. Download Response A"
    }

    fn description(&self) -> &'static str {
        "Saves Response A into task1/responseA: the inline response page when the task embeds it, otherwise its zip or per-file listing plus the model-response text."
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
            // A 4xx means the site is serving a link to a deliverable it
            // doesn't actually have, so the task can never be completed: click
            // the page's Skip button and restart the chain at workflow 1 on the
            // next task (a user Stop passes through). Any other failure
            // (timeout, DNS, dropped connection) says nothing about whether the
            // file exists, so it surfaces normally rather than throwing a
            // perfectly good task away.
            Err(e) if util::is_missing_file_error(&e) => {
                util::skip_and_restart(ctx, "Response A", e).await
            }
            Err(e) => Err(e),
        }
    }
}

/// The download itself, separated from `run` so every failure -- iframe not
/// found, curl error, bad zip, unusable file listing -- funnels into the
/// skip-and-restart recovery.
async fn fetch(ctx: &mut WorkflowCtx) -> Result<()> {
    let dir = util::current_task_dir(ctx)?.join("responseA");

    ctx.step("open the Response A tab").await?;
    util::activate_response_tab(ctx, "Response A").await?;

    ctx.step("capture Response A").await?;
    util::capture_response(ctx, "Response A", &dir).await
}
