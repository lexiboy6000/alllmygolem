//! Step 7: ask Claude (via the `claude` CLI, run as a subprocess -- see
//! util::ask_claude_for_answers) to read everything in task1, judge every
//! Evaluation Criteria question for both responses AND pick an overall
//! winner, then click the matching Good/Bad buttons plus the Overall
//! Quality (Response A / Response B / Tie) button on the live page.
//!
//! Deliberately does NOT click Submit on its own -- that's a real,
//! irreversible submission on an actual task, so this pauses with a confirm
//! prompt first. Decline it and the ratings stay applied on the page for you
//! to review/adjust by hand; confirm and it clicks Submit for you.

use crate::prelude::*;

use super::util;

pub struct AnswerAndApplyCriteria;

#[async_trait]
impl Workflow for AnswerAndApplyCriteria {
    fn name(&self) -> &'static str {
        "7. Answer + apply evaluation criteria"
    }

    fn description(&self) -> &'static str {
        "Has Claude judge each evaluation criterion for Response A/B from the files in task1, then clicks the matching Good/Bad buttons on the page. Pauses before Submit."
    }

    fn dependencies(&self) -> Vec<&'static str> {
        vec!["6. Save evaluation criteria"]
    }

    fn inputs(&self) -> Vec<InputSpec> {
        vec![InputSpec::optional(
            "task_dir",
            "Task folder name (blank = same as step 1)",
            "",
        )]
    }

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        let task_dir = util::current_task_dir(ctx)?;

        ctx.step("ask Claude to judge each criterion").await?;
        util::ask_claude_for_answers(ctx, &task_dir, None).await?;
        let answers_path = task_dir.join("claude_answers");
        if !answers_path.exists() {
            return Err(ctx
                .stop_and_warn(format!(
                    "claude ran but didn't write {}. Check the claude CLI is installed and on \
                     PATH (Settings > Claude path), and that it has permission to write files.",
                    answers_path.display()
                ))
                .await);
        }
        let answers = util::read_claude_answers(&answers_path)?;
        ctx.output(format!(
            "got {} criterion answer(s) + an overall pick from claude",
            answers.criteria.len()
        ));

        ctx.step("apply answers on the page").await?;
        let (applied, missed) = util::apply_answers(ctx, &answers).await?;
        ctx.output(format!("clicked {applied} button(s)"));
        if !missed.is_empty() {
            ctx.warn(format!("couldn't find/click: {}", missed.join(", ")));
        }

        // In the full pipeline the chain carries a `defer_submit` input: the
        // review workflow (step 8) owns the submission, gated behind a human
        // sign-off -- so this workflow must NOT touch Submit at all.
        if ctx.input("defer_submit").is_some_and(|v| !v.trim().is_empty()) {
            ctx.output(
                "submission deferred: the review workflow submits after the human signs off.",
            );
            return Ok(WorkflowOutcome::Completed);
        }

        ctx.step("confirm submit").await?;
        let go = ctx
            .confirm(format!(
                "Applied {applied} rating(s) ({} missed). Review the page, then confirm to click \
                 Submit.",
                missed.len()
            ))
            .await?;
        if !go {
            ctx.output("stopped before Submit (declined).");
            return Ok(WorkflowOutcome::Completed);
        }
        let clicked = util::click_submit_if_enabled(ctx).await?;
        if clicked {
            ctx.output("clicked Submit.");
        } else {
            ctx.warn("Submit button wasn't found or is still disabled -- check the page manually.");
        }

        Ok(WorkflowOutcome::Completed)
    }
}
