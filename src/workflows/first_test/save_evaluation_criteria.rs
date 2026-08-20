//! Step 6: create task1/task_data/evaluation_criteria and save EVERYTHING the
//! rating UI asks, in readable text files inside it:
//!
//! - `questions` -- the numbered Good/Bad Evaluation Criteria list (the
//!   "1. ...", "2. ..." rows under `div.divide-y.divide-border`). Written
//!   EMPTY when the page has no such list, which tells step 7 (and its Claude
//!   prompt) there are no per-criterion Good/Bad ratings;
//! - `comparison_questions` -- the multi-question A/B/Tie rubric, when the
//!   page shows one (each row's heading, judging rule and option set);
//! - `open_feedback_questions` -- the required freeform question(s), when any.
//!
//! The last two are the RECORD of what the page asked: step 7 re-probes the
//! live page for the same questions right before the judging run (the page is
//! the source of truth at that moment) and inlines them into Claude's prompt,
//! so these files and the prompt are built from the same formatting helpers
//! and always agree.
//!
//! On the newest arena layout the rating UI is not on the page to begin with:
//! it lives in a slide-over panel collapsed to an icon on the right, above the
//! words "Evaluation criteria". `util::ensure_criteria_panel_open` clicks that
//! icon first; on older layouts everything is inline and the call does nothing.

use crate::prelude::*;

use super::util;

pub struct SaveEvaluationCriteria;

#[async_trait]
impl Workflow for SaveEvaluationCriteria {
    fn name(&self) -> &'static str {
        "6. Save evaluation criteria"
    }

    fn description(&self) -> &'static str {
        "Saves everything the rating UI asks (Good/Bad criteria, comparison rubric, open \
         feedback questions) to task1/task_data/evaluation_criteria/."
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
        // inline, where this reports "open" immediately with nothing clicked.
        //
        // Retried, because this step can begin while the SPA is still
        // painting: a single look that finds neither the panel nor its rail
        // would otherwise leave the panel shut, and the criteria wait below
        // does not open it, so the step would sit out its whole timeout and
        // halt on a page that was merely a beat late.
        ctx.step("open the evaluation criteria panel").await?;
        let mut panel_ready = false;
        for _ in 0..5 {
            if util::ensure_criteria_panel_open(ctx).await? {
                panel_ready = true;
                break;
            }
            ctx.human_pause(400, 900).await?;
        }
        if !panel_ready {
            ctx.warn(
                "couldn't open the evaluation criteria panel -- looking for the criteria on the \
                 page as-is",
            );
        }

        ctx.step("read evaluation criteria").await?;
        let path = dir.join("questions");
        let mut had_criteria = false;
        match util::wait_for_evaluation_criteria(ctx, Duration::from_secs(15)).await? {
            util::CriteriaLookup::Found(text) => {
                std::fs::write(&path, &text)
                    .map_err(|e| GolemError::Io(format!("write {}: {e}", path.display())))?;
                ctx.output(format!(
                    "saved {} criteria -> {}",
                    text.lines().count(),
                    path.display()
                ));
                had_criteria = true;
            }
            util::CriteriaLookup::NoneOnTask => {
                std::fs::write(&path, "")
                    .map_err(|e| GolemError::Io(format!("write {}: {e}", path.display())))?;
            }
            util::CriteriaLookup::PageNotReady => {
                return Err(ctx.halt(
                    "couldn't find the Evaluation Criteria list (or the Overall Quality card) \
                     on the page after waiting 15s. Make sure you're on a loaded task page, and \
                     that the evaluation-criteria panel opens from the icon on the right.",
                ));
            }
        }

        // The Good/Bad list is only one of the shapes the rating UI takes.
        // Record the others too, so the task folder reflects everything the
        // page actually asked: the multi-question A/B/Tie rubric (which
        // replaces the criteria list on some layouts and accompanies it on
        // others) and any required open
        // feedback questions. Step 7 re-probes the live page for these same
        // questions before the judging run; these files are the durable
        // record, written with the same formatting the prompt uses.
        ctx.step("record comparison + feedback questions").await?;
        let comparisons = util::comparison_questions(ctx).await?;
        if !comparisons.is_empty() {
            let p = dir.join("comparison_questions");
            std::fs::write(&p, util::comparison_question_lines(&comparisons))
                .map_err(|e| GolemError::Io(format!("write {}: {e}", p.display())))?;
            ctx.output(format!(
                "saved {} comparison question(s) -> {}",
                comparisons.len(),
                p.display()
            ));
        }
        let feedback = util::open_feedback_questions(ctx).await?;
        if !feedback.is_empty() {
            let p = dir.join("open_feedback_questions");
            std::fs::write(&p, util::feedback_question_lines(&feedback))
                .map_err(|e| GolemError::Io(format!("write {}: {e}", p.display())))?;
            ctx.output(format!(
                "saved {} open feedback question(s) -> {}",
                feedback.len(),
                p.display()
            ));
        }
        if !had_criteria {
            if !comparisons.is_empty() {
                ctx.output(
                    "no Good/Bad criteria list on this task -- its rubric is the comparison \
                     questions above (wrote an empty questions file)",
                );
            } else {
                ctx.output(
                    "this task has no Evaluation Criteria -- only the Overall Quality pick \
                     (and any feedback noted above) is required (wrote an empty questions \
                     file)",
                );
            }
        }

        Ok(WorkflowOutcome::Completed)
    }
}
