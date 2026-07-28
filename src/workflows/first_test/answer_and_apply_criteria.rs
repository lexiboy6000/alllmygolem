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
            return Err(util::halt_unless_auto(ctx, format!(
                    "claude ran but didn't write {}. Check the claude CLI is installed and on \
                     PATH (Settings > Claude path), and that it has permission to write files.",
                    answers_path.display()
                ))
                .await);
        }
        let answers = util::read_claude_answers(&answers_path)?;
        if answers.criteria.is_empty() {
            ctx.output("no evaluation criteria on this task -- claude judged overall quality only");
        } else {
            ctx.output(format!(
                "got {} criterion answer(s) + an overall pick from claude",
                answers.criteria.len()
            ));
        }

        ctx.step("apply answers on the page").await?;
        // Paced: a ~2-minute jittered break after every few selections. The
        // pacing is timing-only -- the values clicked still come verbatim
        // from claude_answers, and each click verifies itself.
        let (applied, missed) = util::apply_answers(ctx, &answers, true).await?;
        ctx.output(format!("clicked {applied} button(s)"));
        if !missed.is_empty() {
            ctx.warn(format!("couldn't find/click: {}", missed.join(", ")));
        }

        // The long breaks give the SPA extra chances to re-render and drop a
        // selection, so re-check every answer against the live page and fix
        // anything that got lost. The fix-up re-apply is unpaced and cheap:
        // click_until_selected sees already-selected buttons and skips them,
        // so only the lost ones get clicked again.
        ctx.step("verify the applied answers").await?;
        let mut wrong = util::verify_answers_applied(ctx, &answers).await?;
        if !wrong.is_empty() {
            ctx.warn(format!(
                "{} answer(s) didn't stick ({}) -- re-applying them",
                wrong.len(),
                wrong.join(", ")
            ));
            let _ = util::apply_answers(ctx, &answers, false).await?;
            wrong = util::verify_answers_applied(ctx, &answers).await?;
        }
        if wrong.is_empty() {
            ctx.output("verified: every answer on the page matches claude_answers");
        } else {
            ctx.warn(format!(
                "still not selected after re-applying: {} -- check the page before \
                 submitting",
                wrong.join(", ")
            ));
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
        let go = if util::auto_mode(ctx) {
            ctx.warn(format!(
                "automatic mode: submitting {applied} rating(s) ({} missed) without the \
                 confirm prompt.",
                missed.len()
            ));
            true
        } else {
            ctx.confirm(format!(
                "Applied {applied} rating(s) ({} missed). Review the page, then confirm to click \
                 Submit.",
                missed.len()
            ))
            .await?
        };
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
