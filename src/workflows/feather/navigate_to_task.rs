//! "Navigate to task" — accept a task URL, navigate to it, and confirm the task
//! page loaded (both the "Prompt definition" and "Task execution" tabs present).

use crate::prelude::*;

use super::util;

pub struct NavigateToTask;

#[async_trait]
impl Workflow for NavigateToTask {
    fn name(&self) -> &'static str {
        "Navigate to task"
    }
    fn description(&self) -> &'static str {
        "Go to a task URL (.../tasks/<id>/stage/execution) and verify it loaded."
    }
    fn dependencies(&self) -> Vec<&'static str> {
        vec!["Navigate and verify integrity"]
    }
    fn inputs(&self) -> Vec<InputSpec> {
        vec![InputSpec::optional(
            "task_url",
            "Task URL (blank = verify the page already open)",
            "",
        )]
    }

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        let url = ctx
            .input("task_url")
            .map(str::to_string)
            .filter(|u| !u.trim().is_empty());

        match url {
            Some(url) => {
                if !url.contains("feather.openai.com/tasks/") {
                    return Err(ctx
                        .stop_and_warn(format!("not a feather task URL: {url}"))
                        .await);
                }
                ctx.step("navigate to task").await?;
                ctx.navigate(&url).await?;
                ctx.wait_for_default("body").await?;
                ctx.human_pause(900, 1700).await?;
            }
            None => {
                ctx.step("use the currently open page").await?;
            }
        }

        ctx.step("verify task tabs").await?;
        // SPA pages render the tabs a beat after navigation; wait (poll) instead
        // of a one-shot check, and match leniently against any [role=tab].
        let wait = Duration::from_millis(ctx.settings.default_wait_timeout_ms);
        let has_prompt = util::wait_for_text(ctx, "[role=\"tab\"]", "Prompt definition", wait).await?;
        let has_exec = has_prompt
            && util::wait_for_text(ctx, "[role=\"tab\"]", "Task execution", wait).await?;

        if has_prompt && has_exec {
            ctx.output("task page verified ('Prompt definition' + 'Task execution' present)");
            if let Ok(current) = ctx.current_url().await {
                let _ = ctx.set("task_url", current);
            }
            Ok(WorkflowOutcome::Completed)
        } else {
            Err(ctx
                .stop_and_warn(format!(
                    "task page is missing expected tabs (Prompt definition={has_prompt}, \
                     Task execution={has_exec})"
                ))
                .await)
        }
    }
}
