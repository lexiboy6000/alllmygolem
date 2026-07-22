//! Step 3: back out to task1, create responseA/.

use crate::prelude::*;

use super::util;

pub struct CreateResponseADir;

#[async_trait]
impl Workflow for CreateResponseADir {
    fn name(&self) -> &'static str {
        "3. Create responseA directory"
    }

    fn description(&self) -> &'static str {
        "Creates task1/responseA."
    }

    fn dependencies(&self) -> Vec<&'static str> {
        vec!["2. Save task data"]
    }

    fn inputs(&self) -> Vec<InputSpec> {
        vec![InputSpec::optional(
            "task_dir",
            "Task folder name (blank = same as step 1)",
            "",
        )]
    }

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        let dir = util::current_task_dir(ctx)?.join("responseA");
        ctx.step("create responseA").await?;
        std::fs::create_dir_all(&dir)
            .map_err(|e| GolemError::Io(format!("mkdir {}: {e}", dir.display())))?;
        ctx.output(format!("created {}", dir.display()));
        Ok(WorkflowOutcome::Completed)
    }
}
