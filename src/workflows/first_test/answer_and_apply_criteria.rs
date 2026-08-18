//! Step 7: ask Claude (via the `claude` CLI, run as a subprocess -- see
//! util::ask_claude_for_answers) to read everything in task1, judge every
//! Evaluation Criteria question for both responses AND pick an overall
//! winner, then click the matching Good/Bad buttons plus the Overall
//! Quality (Response A / Response B / Tie) button on the live page. Tasks
//! that also ask required open feedback question(s) -- freeform textareas
//! with a minimum length that gate the submit -- get those answered too:
//! the questions are read off the page before the judging run, Claude writes
//! the answers into claude_answers, and apply types them in for real (the
//! page blocks and counts paste attempts).
//!
//! In the pipeline the chain sets `defer_submit`, so this workflow stops after
//! applying the ratings and step 8 owns the (real, irreversible) submission.
//! Run standalone with `defer_submit` cleared, it submits directly -- there is
//! no confirmation prompt, because the pipeline runs unattended.

use crate::prelude::*;

use super::util;

pub struct AnswerAndApplyCriteria;

#[async_trait]
impl Workflow for AnswerAndApplyCriteria {
    fn name(&self) -> &'static str {
        "7. Answer + apply evaluation criteria"
    }

    fn description(&self) -> &'static str {
        "Has Claude judge each evaluation criterion for Response A/B from the files in task1, then clicks the matching Good/Bad buttons and types any required open feedback on the page. Step 8 submits."
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
        // Some tasks also ask required open feedback question(s) -- freeform
        // textareas that gate the submit -- and some replace the Good/Bad
        // criteria with a multi-question comparison rubric (per-question
        // Response A/B/Tie picks). Read both off the live page first so the
        // judging prompt asks for everything the page requires; anything
        // unanswered keeps the submit disabled and step 8 cannot finish.
        let feedback_questions = util::open_feedback_questions(ctx).await?;
        if !feedback_questions.is_empty() {
            ctx.output(format!(
                "this task also asks {} open feedback question(s) -- claude will write the \
                 answer(s)",
                feedback_questions.len()
            ));
        }
        let comparison_questions = util::comparison_questions(ctx).await?;
        if !comparison_questions.is_empty() {
            ctx.output(format!(
                "this task uses a comparison rubric with {} question(s) -- claude will pick \
                 a winner for each",
                comparison_questions.len()
            ));
        }
        util::ask_claude_for_answers(ctx, &task_dir, &feedback_questions, &comparison_questions)
            .await?;
        let answers_path = task_dir.join("claude_answers");
        if !answers_path.exists() {
            return Err(util::halt_now(ctx, format!(
                    "claude ran but didn't write {}. Check the claude CLI is installed and on \
                     PATH (Settings > Claude path), and that it has permission to write files.",
                    answers_path.display()
                ))
                .await);
        }
        // Optionally vary the open-feedback wording through the local
        // rewriter (llama.cpp + Qwen3) before anything reads or types it:
        // rewritten sentence by sentence, mechanically guarded, checked by
        // the same local model, and persisted back into claude_answers so
        // every later read sees one consistent text. Skips itself cleanly
        // when no rewriter is running; never fails the round.
        if !feedback_questions.is_empty() {
            ctx.step("vary the open feedback wording").await?;
            super::vary_feedback::vary_open_feedback(ctx, &task_dir, &feedback_questions)
                .await?;
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

        // In the full pipeline the chain carries a `defer_submit` input: step 8
        // owns the submission (after waiting out the task timer) -- so this
        // workflow must NOT touch Submit at all.
        if ctx.input("defer_submit").is_some_and(|v| !v.trim().is_empty()) {
            ctx.output(
                "submission deferred: step 8 submits once the task timer is satisfied.",
            );
            return Ok(WorkflowOutcome::Completed);
        }

        // No confirm prompt: the pipeline runs unattended, so this submits.
        // Reached only when `defer_submit` is empty, i.e. this workflow was run
        // standalone -- in the pipeline step 8 owns the submission and the
        // early return above fires first.
        ctx.step("submit").await?;
        ctx.warn(format!(
            "submitting {applied} rating(s) ({} missed) with no confirmation prompt",
            missed.len()
        ));
        let clicked = util::submit_evaluation(ctx, &answers).await?;
        if clicked {
            ctx.output("submitted the evaluation.");
        } else {
            ctx.warn(
                "the submit control (\"Save & Continue\", or Submit on older layouts) wasn't \
                 found or is still disabled, or the page's confirmation dialog couldn't be \
                 answered (see above) -- check the page manually.",
            );
        }

        Ok(WorkflowOutcome::Completed)
    }
}
