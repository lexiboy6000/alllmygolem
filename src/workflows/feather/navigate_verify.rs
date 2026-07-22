//! "Navigate and verify integrity" — open feather.openai.com and check the
//! deployed git SHA matches the expected value.

use crate::prelude::*;

pub struct NavigateVerify;

#[async_trait]
impl Workflow for NavigateVerify {
    fn name(&self) -> &'static str {
        "Navigate and verify integrity"
    }
    fn description(&self) -> &'static str {
        "Open feather.openai.com and verify the deployed data-git-sha."
    }
    fn inputs(&self) -> Vec<InputSpec> {
        // The pinned SHA from the spec; overridable without recompiling.
        vec![InputSpec::optional(
            "expected_sha",
            "Expected data-git-sha",
            "783e85f8fe5",
        )]
    }

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        ctx.step("navigate to feather.openai.com").await?;
        ctx.navigate("https://feather.openai.com").await?;
        ctx.wait_for_default("html").await?;

        ctx.step("verify git SHA").await?;
        let expected = ctx
            .input("expected_sha")
            .map(str::to_string)
            .unwrap_or_else(|| "783e85f8fe5".to_string());
        let sha = ctx.attr("html", "data-git-sha").await?;

        match sha.as_deref() {
            Some(s) if s == expected => {
                ctx.output(format!("git SHA OK: {s}"));
                ctx.set("git_sha", s)?;
                Ok(WorkflowOutcome::Completed)
            }
            other => {
                let got = other.unwrap_or("<missing>").to_string();
                // Spec: "If sha does not match, STOP and prompt user." Offer to
                // adopt the new SHA as the persisted default.
                let choice = ctx
                    .choose(
                        format!(
                            "git SHA mismatch.\nExpected: {expected}\nGot: {got}\n\nWhat would you like to do?"
                        ),
                        vec![
                            format!("Make {got} the new default and continue"),
                            "Continue once (keep current default)".to_string(),
                            "Stop".to_string(),
                        ],
                    )
                    .await?;
                match choice {
                    0 => {
                        ctx.set_default_input("Navigate and verify integrity", "expected_sha", &got);
                        ctx.output(format!("updated default expected_sha to {got}"));
                        ctx.set("git_sha", got)?;
                        Ok(WorkflowOutcome::Completed)
                    }
                    1 => {
                        ctx.warn(format!("proceeding once despite SHA mismatch (got {got})"));
                        ctx.set("git_sha", got)?;
                        Ok(WorkflowOutcome::Completed)
                    }
                    _ => Err(ctx.halt(format!(
                        "git SHA mismatch (expected {expected}, got {got})"
                    ))),
                }
            }
        }
    }
}
