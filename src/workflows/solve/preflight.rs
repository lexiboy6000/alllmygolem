//! "Solve: preflight" — verify every dependency the solve pipeline needs, up
//! front, with a specific STOP message on the first gap.

use std::time::Duration;

use crate::prelude::*;

use super::util;

pub struct SolvePreflight;

#[async_trait]
impl Workflow for SolvePreflight {
    fn name(&self) -> &'static str {
        "Solve: preflight"
    }
    fn description(&self) -> &'static str {
        "Check Claude Code (+ auth), Docker (+ daemon + ngspice image), and the task bundle."
    }
    fn requires_browser(&self) -> bool {
        false
    }
    fn inputs(&self) -> Vec<InputSpec> {
        vec![InputSpec::optional(
            "task_id",
            "Task id (blank = newest bundle)",
            "",
        )]
    }

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        let claude = util::claude_bin(&ctx.settings);

        // 1. Claude Code present.
        ctx.step("check Claude Code").await?;
        match ctx.run(&claude, &["--version"], None, Some(Duration::from_secs(20))).await {
            Ok(o) if o.success() => ctx.output(format!("claude: {}", o.stdout.trim())),
            _ => {
                return Err(ctx
                    .stop_and_warn(format!(
                        "Claude Code not found (tried '{claude}'). Install it or set the claude \
                         path in Settings."
                    ))
                    .await);
            }
        }

        // 2. Claude Code actually runs (auth / quota / network).
        ctx.step("check Claude Code auth").await?;
        match ctx
            .run(
                &claude,
                &["-p", "Reply with the single word OK.", "--dangerously-skip-permissions"],
                None,
                Some(Duration::from_secs(90)),
            )
            .await
        {
            Ok(o) if o.success() && !o.combined().trim().is_empty() => {
                ctx.output("Claude Code responded (auth OK)")
            }
            Ok(o) => {
                return Err(ctx
                    .stop_and_warn(format!(
                        "Claude Code ran but failed (login/quota/network?). Output: {}",
                        o.combined().trim()
                    ))
                    .await);
            }
            Err(e) => {
                return Err(ctx
                    .stop_and_warn(format!("Claude Code could not run: {e}"))
                    .await);
            }
        }

        // 3. Docker present + daemon reachable.
        ctx.step("check Docker").await?;
        if !ran_ok(ctx, "docker", &["--version"], 20).await {
            return Err(ctx.stop_and_warn("Docker not found on PATH.").await);
        }
        if !ran_ok(ctx, "docker", &["info"], 25).await {
            return Err(ctx
                .stop_and_warn(
                    "Docker daemon not reachable. Start Docker (and ensure your user can use it, \
                     e.g. the 'docker' group).",
                )
                .await);
        }

        // 4. ngspice image present — build it from the Dockerfile if missing.
        ctx.step("ensure ngspice image").await?;
        let image = util::image_tag(&ctx.settings);
        if !util::image_exists(ctx).await {
            ctx.output(format!(
                "image '{image}' missing; building it from the Dockerfile (first build downloads packages)..."
            ));
            if let Err(e) = util::build_image(ctx).await {
                return Err(ctx.stop_and_warn(e.to_string()).await);
            }
            ctx.output(format!("built image '{image}'"));
        }

        // 5. Image really contains ngspice + gnuplot.
        ctx.step("verify ngspice + gnuplot in image").await?;
        if !ran_ok(
            ctx,
            "docker",
            &[
                "run",
                "--rm",
                image.as_str(),
                "sh",
                "-c",
                "ngspice -v >/dev/null 2>&1 && gnuplot --version",
            ],
            90,
        )
        .await
        {
            return Err(ctx
                .stop_and_warn(format!(
                    "image '{image}' is missing ngspice or gnuplot. Delete it and re-run preflight \
                     to rebuild."
                ))
                .await);
        }

        // 6. Task bundle present + readable.
        ctx.step("check task bundle").await?;
        let task_id = ctx.input("task_id").map(str::to_string);
        match util::find_bundle(&ctx.settings, task_id.as_deref()) {
            Ok((id, bundle)) => {
                if bundle.prompt.trim().is_empty() {
                    ctx.warn("task bundle has an empty prompt");
                }
                ctx.output(format!(
                    "task bundle OK: {id} ({} reference file(s))",
                    bundle.reference_files.len()
                ));
                ctx.set("solve_task_id", id)?;
            }
            Err(e) => {
                return Err(ctx.stop_and_warn(format!("task bundle problem: {e}")).await);
            }
        }

        ctx.output("preflight passed - ready to solve");
        Ok(WorkflowOutcome::Completed)
    }
}

/// Run a command, returning true only if it spawned and exited 0.
async fn ran_ok(ctx: &WorkflowCtx, program: &str, args: &[&str], timeout_secs: u64) -> bool {
    matches!(
        ctx.run(program, args, None, Some(Duration::from_secs(timeout_secs))).await,
        Ok(o) if o.success()
    )
}
