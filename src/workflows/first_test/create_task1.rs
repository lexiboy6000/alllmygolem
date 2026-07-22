//! Step 1: create a new, auto-numbered task directory (task1, task2, task3,
//! ...) so old runs never get overwritten. Pure filesystem work -- no
//! browser calls of its own -- but the rest of the chain does need Chrome
//! connected, so Connect still matters if you run this as part of the full
//! pipeline.

use crate::prelude::*;

use super::util;

pub struct CreateTask1;

#[async_trait]
impl Workflow for CreateTask1 {
    fn name(&self) -> &'static str {
        "1. Create task1 directory"
    }

    fn description(&self) -> &'static str {
        "Creates a new task folder: auto-increments task1, task2, task3, ... (blank task_dir field), or an exact name if you type one."
    }

    fn requires_browser(&self) -> bool {
        false
    }

    fn inputs(&self) -> Vec<InputSpec> {
        vec![InputSpec::optional(
            "task_dir",
            "Task folder name (blank = auto: task1, task2, ...)",
            "",
        )]
    }

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        ctx.step("create new task directory").await?;
        let dir = util::resolve_or_create_task_dir(ctx)?;
        ctx.output(format!("created {}", dir.display()));
        Ok(WorkflowOutcome::Completed)
    }
}
