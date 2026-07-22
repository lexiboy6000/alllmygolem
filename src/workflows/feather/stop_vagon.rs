//! "Stop Vagon" — on the feather task's Task-execution tab, shut down the Vagon
//! computer (the "Vagon computer" card whose Status reads `ready` while running),
//! then poll the status (using the card's Refresh icon) until it is FULLY `off`.
//!
//! It must not stop at `turning_off`: a snapshot/sync only works once the machine
//! is actually off, so a chained "stop then sync" needs the real off state.

use std::time::Duration;

use rand::RngExt;

use crate::prelude::*;

use super::util;

pub struct StopVagon;

#[async_trait]
impl Workflow for StopVagon {
    fn name(&self) -> &'static str {
        "Stop Vagon"
    }
    fn description(&self) -> &'static str {
        "Shut down the Vagon computer from the feather task page and wait until it's fully off."
    }
    fn inputs(&self) -> Vec<InputSpec> {
        vec![InputSpec::required(
            "task_url",
            "Task URL (.../tasks/<id>/stage/execution)",
        )]
    }

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        let wait = Duration::from_millis(ctx.settings.default_wait_timeout_ms);
        // A Windows VM shutdown can take a few minutes (Vagon can lag) — be patient.
        let shutdown = wait.max(Duration::from_secs(600));

        let task_url = ctx.require_input("task_url")?;

        // --- open the task on the Task execution tab (where the Vagon card lives) ---
        ctx.step("open task execution").await?;
        ctx.navigate(&task_url).await?;
        ctx.wait_for_default("body").await?;
        if !util::wait_for_text(ctx, "[role=\"tab\"]", "Task execution", wait).await? {
            return Err(ctx
                .stop_and_warn(
                    "Not on a loaded task page (no 'Task execution' tab). Make sure you're logged \
                     in and the URL is a task's stage page.",
                )
                .await);
        }
        if !util::click_contains(ctx, "[role=\"tab\"]", "Task execution").await? {
            return Err(ctx.stop_and_warn("Could not click the 'Task execution' tab.").await);
        }
        ctx.human_pause(800, 1600).await?;

        // --- read the current Vagon status ---
        ctx.step("read Vagon status").await?;
        let status = match wait_for_status(ctx, wait).await? {
            Some(s) => s,
            None => {
                return Err(ctx
                    .stop_and_warn("Couldn't find the Vagon computer 'Status' on the Task execution tab.")
                    .await);
            }
        };
        ctx.output(format!("Vagon status: {status}"));
        if is_fully_off(&status) {
            ctx.warn_user(format!("Vagon computer is already off ('{status}') — nothing to do."))
                .await?;
            return Ok(WorkflowOutcome::CompletedWith(json!({ "status": status })));
        }

        // --- click Shut down (unless it's already shutting down) ---
        if is_turning_off(&status) {
            ctx.note_status(format!("already '{status}' — waiting for it to finish shutting down…"));
        } else {
            ctx.step("shut down").await?;
            if !util::wait_for_text(ctx, "button", "Shut down", wait).await? {
                return Err(ctx
                    .stop_and_warn("Could not find the 'Shut down' button on the Vagon computer card.")
                    .await);
            }
            if !util::click_contains(ctx, "button", "Shut down").await? {
                return Err(ctx.stop_and_warn("Could not click 'Shut down'.").await);
            }
            ctx.human_pause(900, 1700).await?;
        }

        // --- poll (refreshing) until the machine is FULLY off ---
        // The status doesn't live-update; click the card's Refresh icon to re-read.
        // `turning_off` is NOT done — only `off`/`stopped` counts.
        ctx.step("wait for off").await?;
        let deadline = tokio::time::Instant::now() + shutdown;
        let final_status = loop {
            ctx.guard().await?;
            let s = util::vagon_status(ctx).await?.unwrap_or_default();
            if is_fully_off(&s) {
                break s;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ctx
                    .stop_and_warn(format!(
                        "The Vagon computer never reached 'off' (status is still '{s}') — the \
                         shutdown is taking longer than expected. Refresh the card and check it \
                         before syncing."
                    ))
                    .await);
            }
            ctx.note_status(format!("status '{s}' — waiting for off…"));
            let _ = util::refresh_vagon_status(ctx).await;
            // Refresh on a relaxed, human cadence — a shutdown takes minutes, so
            // there's no point polling every couple of seconds.
            long_pause(ctx, 25_000, 45_000).await?;
        };

        ctx.output(format!("Vagon status: {final_status}"));
        ctx.warn_user(format!(
            "Vagon computer is now off ('{final_status}') — safe to sync."
        ))
        .await?;

        Ok(WorkflowOutcome::CompletedWith(json!({ "status": final_status })))
    }
}

/// Sleep a relaxed `min_ms..=max_ms` interval in short, cancellable chunks, so a
/// STOP/pause stays responsive across the multi-minute shutdown wait (unlike a
/// single long `human_pause`, which can't be interrupted mid-sleep).
async fn long_pause(ctx: &WorkflowCtx, min_ms: u64, max_ms: u64) -> Result<()> {
    let mut left = rand::rng().random_range(min_ms..=max_ms);
    while left > 0 {
        ctx.guard().await?;
        let chunk = left.min(1000);
        tokio::time::sleep(Duration::from_millis(chunk)).await;
        left -= chunk;
    }
    ctx.guard().await
}

/// Poll until the Vagon status card renders a value, or `timeout`.
async fn wait_for_status(ctx: &WorkflowCtx, timeout: Duration) -> Result<Option<String>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        ctx.guard().await?;
        if let Some(s) = util::vagon_status(ctx).await? {
            return Ok(Some(s));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        ctx.human_pause(250, 500).await?;
    }
}

/// The machine is COMPLETELY off (shutdown finished) — `off`/`stopped`, but NOT
/// the transitional `turning_off`/`turning_on`.
fn is_fully_off(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    if l.contains("turn") {
        return false; // turning_on / turning_off are transitional
    }
    l.contains("off") || l.contains("stop")
}

/// The machine is mid-shutdown (`turning_off`) — don't click Shut down again, just
/// wait it out.
fn is_turning_off(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.contains("turn") && l.contains("off")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fully_off_excludes_transitional() {
        assert!(is_fully_off("off"));
        assert!(is_fully_off("stopped"));
        assert!(is_fully_off("stop"));
        // Transitional states are NOT "off".
        assert!(!is_fully_off("turning_off"));
        assert!(!is_fully_off("turning_on"));
        assert!(!is_fully_off("ready"));
        assert!(!is_fully_off("connecting"));
    }

    #[test]
    fn turning_off_only_matches_shutdown_transition() {
        assert!(is_turning_off("turning_off"));
        assert!(is_turning_off("Turning Off"));
        assert!(!is_turning_off("turning_on"));
        assert!(!is_turning_off("off"));
        assert!(!is_turning_off("ready"));
    }
}
