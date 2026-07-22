//! "Solve task (Claude + Docker)" — provision an ngspice container, have Claude
//! Code solve the netlist (testing in the container), gate completion with two
//! independent review agents, and finalize the product. Auto-retries with
//! reviewer feedback up to `solve_max_iterations`, then STOPs and prompts.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::prelude::*;

use super::util;

pub struct SolveTask;

#[async_trait]
impl Workflow for SolveTask {
    fn name(&self) -> &'static str {
        "Solve task (Claude + Docker)"
    }
    fn description(&self) -> &'static str {
        "Solve the downloaded SPICE task with Claude Code in a Docker ngspice sandbox, gated by review agents."
    }
    fn dependencies(&self) -> Vec<&'static str> {
        vec!["Solve: preflight"]
    }
    fn requires_browser(&self) -> bool {
        false
    }
    fn inputs(&self) -> Vec<InputSpec> {
        vec![InputSpec::optional("task_id", "Task id (blank = newest bundle)", "")]
    }

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        let task_id = ctx.input("task_id").map(str::to_string);
        let (id, bundle) = util::find_bundle(&ctx.settings, task_id.as_deref())?;
        ctx.output(format!("solving task {id}"));

        // --- workspace ---
        ctx.step("set up workspace").await?;
        let ws = ctx.settings.output_dir.join("solve").join(&id);
        let container = util::container_name(&id);
        util::stage_workspace(ctx, &ws, &bundle, &container)?;

        // --- provision container ---
        ctx.step("start ngspice container").await?;
        let image = util::image_tag(&ctx.settings);
        let ws_abs = util::absolute(&ws)?;
        let mount = format!("{}:/work", ws_abs.display());
        // Remove any stale container with this name first (ignore failure).
        let _ = ctx
            .run("docker", &["rm", "-f", container.as_str()], None, Some(Duration::from_secs(30)))
            .await;
        // Run as the host user so files the container writes (plots, etc.) are
        // owned by you on the host, not root.
        let user = util::host_user();
        let mut run_args: Vec<&str> = vec!["run", "-d", "--name", container.as_str()];
        if let Some(u) = user.as_deref() {
            run_args.push("--user");
            run_args.push(u);
        }
        run_args.push("-v");
        run_args.push(mount.as_str());
        run_args.push(image.as_str());
        run_args.push("sleep");
        run_args.push("infinity");
        let started = ctx
            .run("docker", &run_args, None, Some(Duration::from_secs(120)))
            .await?;
        if !started.success() {
            return Err(ctx
                .stop_and_warn(format!("could not start container: {}", started.combined().trim()))
                .await);
        }

        // --- solve + review loop ---
        let max = ctx.settings.solve_max_iterations.max(1);
        let timeout = Duration::from_secs(ctx.settings.claude_timeout_secs.max(60));
        let mut feedback = String::new();
        let mut success = false;
        let mut last_verdict = String::from("no attempt completed");

        for attempt in 1..=max {
            // Solve.
            ctx.step(format!("solve (attempt {attempt}/{max})")).await?;
            let solve_prompt = if feedback.is_empty() {
                util::SOLVE_PROMPT.to_string()
            } else {
                format!(
                    "{}\n\nA reviewer found these issues to fix:\n{}",
                    util::SOLVE_PROMPT, feedback
                )
            };
            claude_run(ctx, &ws, &solve_prompt, timeout).await?;
            if !ws.join("solution.cir").exists() {
                feedback = "No solution.cir was produced. Create the netlist at solution.cir.".into();
                last_verdict = feedback.clone();
                continue;
            }

            // Review #1 — completeness.
            ctx.step(format!("review solution (attempt {attempt}/{max})")).await?;
            claude_run(ctx, &ws, &util::review_solution_prompt(&container), timeout).await?;
            let r1 = util::read_verdict(&ws.join("review_solve.json"));
            if !r1.complete {
                ctx.output("solution review: not complete");
                feedback = r1.feedback.clone();
                last_verdict = format!("solution review: {}", r1.feedback.trim());
                continue;
            }
            ctx.output("solution review: complete");

            // Plots.
            ctx.step(format!("generate plots (attempt {attempt}/{max})")).await?;
            claude_run(ctx, &ws, &util::plot_prompt(&container), timeout).await?;
            if util::count_pngs(&ws.join("plots")) == 0 {
                feedback = "No plots were produced in plots/. Generate PNGs with ngspice's gnuplot command.".into();
                last_verdict = feedback.clone();
                continue;
            }

            // Review #2 — plots vs rubric.
            ctx.step(format!("review plots (attempt {attempt}/{max})")).await?;
            claude_run(ctx, &ws, &util::review_plots_prompt(&container), timeout).await?;
            let r2 = util::read_verdict(&ws.join("review_plots.json"));
            if r2.complete {
                success = true;
                break;
            }
            ctx.output("plot review: not complete");
            feedback = r2.feedback.clone();
            last_verdict = format!("plot review: {}", r2.feedback.trim());
        }

        // --- finalize ---
        ctx.step("finalize").await?;
        let final_dir = finalize_artifacts(&ws)?;

        if success {
            // Remove the container on success.
            let _ = ctx
                .run("docker", &["rm", "-f", container.as_str()], None, Some(Duration::from_secs(30)))
                .await;
            let netlist = final_dir.join("solution.cir");
            ctx.output(format!("DONE - final netlist: {}", netlist.display()));
            Ok(WorkflowOutcome::CompletedWith(json!({
                "task_id": id,
                "netlist": netlist.display().to_string(),
                "workspace": ws.display().to_string(),
            })))
        } else {
            // Keep the container for inspection on failure.
            Err(ctx
                .stop_and_warn(format!(
                    "Could not satisfy the rubric after {max} attempt(s). Last verdict: {last_verdict}\n\
                     Artifacts: {}\nContainer kept for inspection: {container}",
                    ws.display()
                ))
                .await)
        }
    }
}

/// Run a Claude Code agent in the workspace with full tool autonomy, streaming
/// progress. Uses the configured model + reasoning effort.
async fn claude_run(
    ctx: &WorkflowCtx,
    ws: &Path,
    prompt: &str,
    timeout: Duration,
) -> Result<CommandOutput> {
    let claude = util::claude_bin(&ctx.settings);
    let model = ctx.settings.solve_model.clone();
    let effort = ctx.settings.solve_effort.clone();
    let mut args: Vec<&str> = vec![
        "-p",
        prompt,
        "--dangerously-skip-permissions",
        "--output-format",
        "stream-json",
        "--verbose",
    ];
    if !model.trim().is_empty() {
        args.push("--model");
        args.push(model.as_str());
    }
    if !effort.trim().is_empty() {
        args.push("--effort");
        args.push(effort.as_str());
    }
    ctx.run_claude(&claude, &args, Some(ws), Some(timeout)).await
}

/// Copy the product artifacts into `final/` and return that dir.
fn finalize_artifacts(ws: &Path) -> Result<PathBuf> {
    let final_dir = ws.join("final");
    let plots_final = final_dir.join("plots");
    std::fs::create_dir_all(&plots_final)
        .map_err(|e| GolemError::Io(format!("mkdir final dir: {e}")))?;

    for name in ["review_solve.json", "review_plots.json"] {
        let src = ws.join(name);
        if src.exists() {
            let _ = std::fs::copy(&src, final_dir.join(name));
        }
    }
    // Copy EVERY .cir the agents produced (solution.cir, the plotting deck, any
    // helpers) so the plotting netlist is preserved alongside the submission.
    if let Ok(read) = std::fs::read_dir(ws) {
        for entry in read.flatten() {
            let p = entry.path();
            if p.extension().and_then(|x| x.to_str()) == Some("cir")
                && let Some(fname) = p.file_name() {
                    let _ = std::fs::copy(&p, final_dir.join(fname));
                }
        }
    }
    if let Ok(read) = std::fs::read_dir(ws.join("plots")) {
        for entry in read.flatten() {
            let p = entry.path();
            let is_png = p
                .extension()
                .and_then(|x| x.to_str())
                .map(|x| x.eq_ignore_ascii_case("png"))
                .unwrap_or(false);
            if is_png
                && let Some(fname) = p.file_name() {
                    let _ = std::fs::copy(&p, plots_final.join(fname));
                }
        }
    }
    Ok(final_dir)
}
