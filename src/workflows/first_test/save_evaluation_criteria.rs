//! Step 6: create task1/task_data/evaluation_criteria and save the
//! Evaluation Criteria questions (the numbered "1. ...", "2. ..." list under
//! `div.divide-y.divide-border`) to a readable text file inside it.
//!
//! On the newest arena layout that list is not on the page to begin with: it
//! lives in a slide-over panel collapsed to an icon on the right, above the
//! words "Evaluation criteria". `util::ensure_criteria_panel_open` clicks that
//! icon first; on older layouts the list is inline and the call does nothing.
//!
//! Some tasks have no Evaluation Criteria section at all and only ask for
//! the Overall Quality pick -- for those the questions file is written EMPTY,
//! which tells step 7 (and its Claude prompt) to judge overall quality only.

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

        // On the newest arena layout the criteria live in a slide-over panel
        // that starts collapsed to a rail on the right -- nothing inside it is
        // in the DOM until its icon is clicked. Older layouts render the list
        // inline and this is a no-op.
        ctx.step("open the evaluation criteria panel").await?;
        util::ensure_criteria_panel_open(ctx).await?;

        ctx.step("read evaluation criteria").await?;
        let path = dir.join("questions");
        match util::wait_for_evaluation_criteria(ctx, Duration::from_secs(15)).await? {
            util::CriteriaLookup::Found(text) => {
                std::fs::write(&path, &text)
                    .map_err(|e| GolemError::Io(format!("write {}: {e}", path.display())))?;
                ctx.output(format!(
                    "saved {} criteria -> {}",
                    text.lines().count(),
                    path.display()
                ));
            }
            util::CriteriaLookup::NoneOnTask => {
                std::fs::write(&path, "")
                    .map_err(|e| GolemError::Io(format!("write {}: {e}", path.display())))?;
                ctx.output(
                    "this task has no Evaluation Criteria -- only the Overall Quality pick is \
                     required (wrote an empty questions file)",
                );
            }
            util::CriteriaLookup::PageNotReady => {
                return Err(ctx.halt(
                    "couldn't find the Evaluation Criteria list (or the Overall Quality card) \
                     on the page after waiting 15s. Make sure you're on a loaded task page, and \
                     that the evaluation-criteria panel opens from the icon on the right.",
                ));
            }
        }

        Ok(WorkflowOutcome::Completed)
    }
}
