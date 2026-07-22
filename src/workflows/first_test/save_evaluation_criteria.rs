//! Step 6: create task1/task_data/evaluation_criteria and save the
//! Evaluation Criteria questions (the numbered "1. ...", "2. ..." list under
//! `div.divide-y.divide-border`) to a readable text file inside it.

use crate::prelude::*;

use super::util;

pub struct SaveEvaluationCriteria;

#[async_trait]
impl Workflow for SaveEvaluationCriteria {
    fn name(&self) -> &'static str {
        "6. Save evaluation criteria"
    }

    fn description(&self) -> &'static str {
        "Saves the Evaluation Criteria questions to task1/task_data/evaluation_criteria/questions."
    }

    fn dependencies(&self) -> Vec<&'static str> {
        vec!["5. Download Response B"]
    }

    fn inputs(&self) -> Vec<InputSpec> {
        vec![InputSpec::optional(
            "task_dir",
            "Task folder name (blank = same as step 1)",
            "",
        )]
    }

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        let dir = util::current_task_dir(ctx)?.join("task_data").join("evaluation_criteria");

        ctx.step("create evaluation_criteria directory").await?;
        std::fs::create_dir_all(&dir)
            .map_err(|e| GolemError::Io(format!("mkdir {}: {e}", dir.display())))?;

        ctx.step("read evaluation criteria").await?;
        let text = util::wait_for_evaluation_criteria_text(ctx, Duration::from_secs(15))
            .await?
            .ok_or_else(|| {
                ctx.halt(
                    "couldn't find the Evaluation Criteria list on the page after waiting 15s \
                     (looked for a header span 'Evaluation Criteria' followed by a \
                     div.divide-y.divide-border list). Make sure you're on a loaded task page \
                     with the criteria visible.",
                )
            })?;

        let path = dir.join("questions");
        std::fs::write(&path, &text)
            .map_err(|e| GolemError::Io(format!("write {}: {e}", path.display())))?;
        ctx.output(format!(
            "saved {} criteria -> {}",
            text.lines().count(),
            path.display()
        ));

        Ok(WorkflowOutcome::Completed)
    }
}
