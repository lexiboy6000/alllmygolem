//! "Open VM terminal" — from a feather task page, click *Set task* then
//! *Connect*, attach to the Vagon session tab that pops up, wait for the Windows
//! VM stream to come up, then drive the desktop ENTIRELY by keyboard to open a
//! fullscreen terminal with `nvim <file>.cir` ready — the hand-off point for the
//! "Complete task" typing workflow.
//!
//! Everything inside the VM is a `<video>` pixel stream with no DOM, so the VM
//! steps are blind keystrokes paced by generous human pauses (the Windows key to
//! open Start, type "terminal", Enter; `cd` into the desktop folder by a short
//! UUID prefix + Tab; F11 to fullscreen; `nvim <file>.cir`).

use std::time::Duration;

use rand::RngExt;

use crate::prelude::*;

use super::util;

pub struct OpenVmTerminal;

#[async_trait]
impl Workflow for OpenVmTerminal {
    fn name(&self) -> &'static str {
        "Open VM terminal"
    }
    fn description(&self) -> &'static str {
        "Set task + Connect, attach to the Vagon VM, and open a fullscreen nvim terminal (no submit)."
    }
    fn inputs(&self) -> Vec<InputSpec> {
        vec![
            InputSpec::required("task_url", "Task URL (.../tasks/<id>/stage/execution)"),
            InputSpec::required("filename", "Circuit file name, e.g. LowPassFilter (.cir added if missing)"),
        ]
    }

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        let wait = Duration::from_millis(ctx.settings.default_wait_timeout_ms);
        // The VM boot is advertised as "2-4 minutes", so be very patient there.
        let boot = Duration::from_secs(360);

        let task_url = ctx.require_input("task_url")?;
        let task_id = task_id_from_url(&task_url).ok_or_else(|| {
            GolemError::Input(format!("could not find /tasks/<id>/ in URL: {task_url}"))
        })?;
        // The desktop folder starts with the UUID's first segment; type a short
        // (6-8 char) prefix and Tab-complete it, which also reads as more human.
        let first_segment = task_id.split('-').next().unwrap_or(&task_id).to_string();
        let prefix_len = rand::rng()
            .random_range(6usize..=8)
            .min(first_segment.len().max(1));
        let folder_prefix = first_segment.chars().take(prefix_len).collect::<String>();

        let raw_name = ctx.require_input("filename")?;
        let cir_name = if raw_name.trim().to_ascii_lowercase().ends_with(".cir") {
            raw_name.trim().to_string()
        } else {
            format!("{}.cir", raw_name.trim())
        };

        // --- feather: open the task ON the Task execution tab ---
        // Wait for the SPA tabs to actually render (this is "feather has loaded"),
        // then switch to Task execution: the /stage/execution URL can still come up
        // on the Prompt definition tab, and Set task / Connect only live under Task
        // execution (Radix unmounts the inactive tab, so they aren't in the DOM at
        // all until we switch).
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
            return Err(ctx
                .stop_and_warn("Could not click the 'Task execution' tab.")
                .await);
        }
        ctx.human_pause(800, 1600).await?;

        // --- assign the task (Set task), then Connect to boot + open the VM ---
        // "Set task" ASSIGNS the task to the workstation; the *Connect* button only
        // works AFTER the task is set. Crucially, feather loads the card's buttons
        // DISABLED until it has fetched the workstation status — clicking too early
        // is a no-op (the task never gets set, then Connect never enables). So: wait
        // for the card to become interactive, set the task if it isn't already, and
        // wait for that to register (Connect comes alive) before connecting.
        ctx.step("set task").await?;
        if !util::wait_for_text(ctx, "button", "Set task", wait).await? {
            return Err(ctx
                .stop_and_warn("Could not find the Vagon controls on the Task execution tab.")
                .await);
        }
        ctx.note_status("waiting for the Vagon computer card to finish loading…");
        if !wait_card_interactive(ctx, wait.max(Duration::from_secs(45))).await? {
            return Err(ctx
                .stop_and_warn(
                    "The Vagon computer card never became interactive (Set task / Connect stayed \
                     disabled) — feather didn't finish loading the workstation status. Reload the \
                     task page and retry.",
                )
                .await);
        }
        // If the task isn't assigned yet, Set task is enabled — set it and WAIT for
        // the assignment to take effect (Connect comes alive). If it's already set,
        // Set task is disabled and we skip straight to Connect.
        if util::enabled_contains(ctx, "button", "Set task").await? {
            ctx.note_status("assigning the task (Set task)…");
            if !util::click_enabled_contains(ctx, "button", "Set task").await? {
                return Err(ctx.stop_and_warn("Could not click 'Set task'.").await);
            }
            ctx.step("wait for the task to be set").await?;
            if !wait_button_enabled(ctx, "Connect", wait.max(Duration::from_secs(90))).await? {
                return Err(ctx
                    .stop_and_warn(
                        "After 'Set task', the Connect button never enabled — the task assignment \
                         didn't complete (check the Vagon computer card; there may be a confirm \
                         step or the status is still updating).",
                    )
                    .await);
            }
            ctx.note_status("task set — Connect is now available.");
        }

        // --- open the Vagon session ---
        // "Connect" boots the VM (if off) AND opens the session in a POPUP, which
        // Chromium frequently blocks ("Popups blocked: …vagon…"). The login link is
        // NOT on the card until Connect has run; after a blocked popup, Refresh
        // surfaces it. So: clear stale Vagon tabs, click Connect, wait for the popup;
        // if none appears, Refresh the card to get the login link and open THAT (a
        // real anchor opens reliably), falling back to a direct navigate.
        ctx.step("connect").await?;
        // Clean slate so the only Vagon tab after Connect is the fresh one (so a
        // leftover/expired session from an earlier run can't be mistaken for it).
        let _ = ctx.close_other_targets("app.vagon.io/team/session").await;

        // Connect should be enabled now; poll briefly and click it.
        let mut clicked_connect = false;
        let connect_deadline = tokio::time::Instant::now() + wait.max(Duration::from_secs(30));
        loop {
            ctx.guard().await?;
            if util::click_enabled_contains(ctx, "button", "Connect").await? {
                clicked_connect = true;
                break;
            }
            if tokio::time::Instant::now() >= connect_deadline {
                break;
            }
            ctx.human_pause(400, 800).await?;
        }
        if !clicked_connect {
            return Err(ctx
                .stop_and_warn("The 'Connect' button never became clickable on the Vagon computer card.")
                .await);
        }
        ctx.note_status("clicked Connect — booting/opening the VM (this can take minutes)…");

        // First chance: the popup Connect tries to open.
        let mut attached = ctx
            .switch_to_target("app.vagon.io", "expired", Duration::from_secs(12))
            .await?;

        // Blocked popup → Refresh the card to reveal the login link, then open it.
        if !attached {
            ctx.note_status("no popup (likely blocked) — refreshing to surface the login link…");
            match wait_for_login_link(ctx, boot).await? {
                Some(url) => {
                    let needle = util::vagon_session_needle(&url);
                    ctx.note_status("opening the Vagon session via the login link…");
                    let _ = util::click_visible(ctx, "a[href*=\"app.vagon.io/team/session\"]", wait).await?;
                    attached = ctx
                        .switch_to_target(&needle, "expired", wait.max(Duration::from_secs(15)))
                        .await?;
                    // Anchor blocked too → navigate the current tab directly.
                    if !attached {
                        ctx.note_status("the link didn't open a tab — navigating directly…");
                        ctx.navigate(&url).await?;
                        attached = ctx
                            .switch_to_target(&needle, "expired", wait.max(Duration::from_secs(20)))
                            .await?
                            || ctx.current_url().await.unwrap_or_default().contains("app.vagon.io");
                    }
                }
                None => {
                    return Err(ctx
                        .stop_and_warn(
                            "Connect opened no popup, and no login link appeared after refreshing the \
                             Vagon computer card — can't reach the session.",
                        )
                        .await);
                }
            }
        }
        if !attached {
            return Err(ctx
                .stop_and_warn("Couldn't open the Vagon session (no popup, and the login link wouldn't open).")
                .await);
        }
        // Guarantee a SINGLE desktop: close any OTHER Vagon session tabs (a late
        // popup duplicate) so two streams can't fight over the workstation.
        let _ = ctx.close_other_targets("app.vagon.io/team/session").await;

        // --- wait for the Windows VM stream to be live ---
        // IMPORTANT: keep the controlled page pinned to the Vagon tab. If evals are
        // running on the feather tab instead (e.g. the attach didn't take or the
        // page drifted), the readiness check can never see the stream — so re-attach
        // whenever the active URL isn't Vagon, and log which tab we're actually on.
        ctx.step("wait for VM").await?;
        ctx.note_status("VM is starting up (this can take 2-4 minutes)…");
        let deadline = tokio::time::Instant::now() + boot;
        let mut last_url = String::new();
        loop {
            ctx.guard().await?;
            let url = ctx.current_url().await.unwrap_or_default();
            if !url.contains("app.vagon.io") || url.contains("expired") {
                // Not on the live Vagon tab (or stuck on a stale/expired one) —
                // (re)attach to the newest non-expired Vagon tab before checking.
                let _ = ctx
                    .switch_to_target("app.vagon.io", "expired", wait.max(Duration::from_secs(20)))
                    .await?;
            }
            if url != last_url {
                ctx.note_status(format!("VM tab: {url}"));
                last_url = url;
            }
            if ctx.eval(PLAYER_READY_JS).await?.as_bool().unwrap_or(false) {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ctx
                    .stop_and_warn(
                        "The Vagon VM stream never came up (the boot/connect spinner never cleared, \
                         or evals couldn't reach the Vagon tab). Check the 'VM tab:' status line.",
                    )
                    .await);
            }
            ctx.human_pause(800, 1500).await?;
        }
        // A freshly-BOOTED VM (Connect just powered it on) reaches the streamable
        // state BEFORE the Windows desktop is fully interactive — the Start menu /
        // search won't respond to keystrokes yet. Give it a generous settle so the
        // sequence below doesn't fire into a half-loaded desktop (the classic
        // "only the Start bar opened" symptom).
        ctx.note_status("VM stream is up — waiting for the Windows desktop to finish loading…");
        cold_wait(ctx, 12000, 18000).await?;

        // --- drive the desktop by keyboard ---
        // The Vagon stream only captures keyboard input while its tab is the
        // FOREGROUND/active tab — bring it to the front before any keystrokes.
        ctx.bring_to_front().await?;
        ctx.human_pause(500, 1000).await?;

        // Screenshots after each step so we can SEE what the (no-DOM) VM did. Saved
        // to <output>/vm-shots/ — share these if a step didn't take.
        let shots = ctx.settings.output_dir.join("vm-shots");

        ctx.step("focus the VM").await?;
        // Click an EMPTY part of the stream (right-of-centre, clear of the centered
        // Start menu and desktop icons) so it grabs keyboard capture WITHOUT opening
        // or selecting anything. The stream fills the viewport, so click viewport-
        // relative (more robust than #player's rect, which can momentarily vanish).
        let vp = ctx.eval("[window.innerWidth, window.innerHeight]").await?;
        let vw = vp.as_array().and_then(|a| a.first()).and_then(|v| v.as_f64()).unwrap_or(1280.0);
        let vh = vp.as_array().and_then(|a| a.get(1)).and_then(|v| v.as_f64()).unwrap_or(720.0);
        ctx.click_at(vw * 0.78, vh * 0.5).await?;
        ctx.human_pause(700, 1300).await?;
        // Escape twice to dismiss any leftover Start menu / dialog -> clean desktop.
        ctx.press_key("Escape").await?;
        ctx.human_pause(500, 1000).await?;
        ctx.press_key("Escape").await?;
        ctx.human_pause(700, 1300).await?;
        let _ = ctx.save_screenshot(&shots.join("01-focused.png")).await;

        ctx.step("open terminal").await?;
        // Windows key -> Start (search box auto-focuses). Start now reliably OPENS
        // because we cleaned the state above — a leftover-open Start would make this
        // TOGGLE it shut, which is what dropped "terminal" onto the bare desktop.
        ctx.press_key("Meta").await?;
        // Windows 11 is VERY slow to populate the Start menu + search on a COLD
        // boot — this is exactly why it "always works on the second try" (warm).
        // Wait long and patiently before typing, or the keystrokes hit a not-yet-
        // ready Start menu and do nothing.
        ctx.note_status("opened Start — waiting ~20-30s for the cold-boot Start menu to be ready…");
        cold_wait(ctx, 20000, 30000).await?;
        let _ = ctx.save_screenshot(&shots.join("02-after-win-key.png")).await;
        type_str(ctx, "terminal").await?;
        // The "Terminal" search result also takes a while to become selectable cold.
        ctx.note_status("typed 'terminal' — waiting for the search result to be selectable…");
        cold_wait(ctx, 15000, 25000).await?;
        let _ = ctx.save_screenshot(&shots.join("03-typed-terminal.png")).await;
        ctx.press_key("Enter").await?;
        // Windows Terminal itself is slow to launch on a cold boot.
        ctx.note_status("launching Windows Terminal (cold start is slow)…");
        cold_wait(ctx, 15000, 25000).await?;
        let _ = ctx.save_screenshot(&shots.join("04-after-enter.png")).await;

        // Fullscreen the terminal (Windows Terminal: F11) — important for the
        // subsequent typing workflow to look like a human working fullscreen.
        ctx.press_key("F11").await?;
        ctx.human_pause(1200, 2200).await?;

        ctx.step("open the circuit file").await?;
        // cd into the desktop folder (two steps: Desktop, then the UUID folder).
        type_str(ctx, "cd Desktop").await?;
        ctx.press_key("Enter").await?;
        ctx.human_pause(700, 1400).await?;
        type_str(ctx, &format!("cd {folder_prefix}")).await?;
        ctx.human_pause(400, 900).await?;
        ctx.press_key("Tab").await?; // tab-complete the folder name
        ctx.human_pause(500, 1100).await?;
        ctx.press_key("Enter").await?;
        ctx.human_pause(700, 1400).await?;

        // Open the file in neovim — ready for the typing workflow.
        type_str(ctx, &format!("nvim {cir_name}")).await?;
        ctx.human_pause(400, 900).await?;
        ctx.press_key("Enter").await?;
        ctx.human_pause(1500, 2800).await?;
        let _ = ctx.save_screenshot(&shots.join("05-nvim.png")).await;

        ctx.warn_user(format!(
            "VM terminal sequence done; `nvim {cir_name}` should be open (folder prefix \
             '{folder_prefix}').\n\nThe VM is a pixel stream with no feedback, so I saved \
             screenshots after each step to:\n  {}\nIf the terminal/editor didn't open, check \
             those — they show whether the Start menu opened (02), the search found Terminal (03), \
             it launched (04), and nvim opened (05) — which pinpoints where the keystrokes stopped \
             reaching the VM.",
            shots.display()
        ))
        .await?;

        Ok(WorkflowOutcome::CompletedWith(json!({
            "task_id": task_id,
            "folder_prefix": folder_prefix,
            "file": cir_name,
        })))
    }
}

/// After Connect, the "quick connect" login link only appears once the Vagon
/// computer card is REFRESHED. Click the card's Refresh icon and poll until the
/// link's href shows up (or `timeout`, floored at 30s so a slow card has time).
async fn wait_for_login_link(ctx: &mut WorkflowCtx, timeout: Duration) -> Result<Option<String>> {
    let deadline = tokio::time::Instant::now() + timeout.max(Duration::from_secs(30));
    let mut last_refresh: Option<tokio::time::Instant> = None;
    loop {
        ctx.guard().await?;
        if let Some(url) = util::vagon_login_link(ctx).await? {
            return Ok(Some(url));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        // Refresh the card every few seconds — that's what surfaces the link.
        let due = last_refresh.is_none_or(|t| t.elapsed() >= Duration::from_secs(3));
        if due {
            let _ = util::refresh_vagon_status(ctx).await;
            last_refresh = Some(tokio::time::Instant::now());
        }
        ctx.human_pause(800, 1500).await?;
    }
}

/// Poll until the Vagon computer card is interactive — i.e. EITHER "Set task" or
/// "Connect" is enabled (feather loads them disabled until the workstation status
/// arrives). Returns false on `timeout`.
async fn wait_card_interactive(ctx: &WorkflowCtx, timeout: Duration) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        ctx.guard().await?;
        if util::enabled_contains(ctx, "button", "Set task").await?
            || util::enabled_contains(ctx, "button", "Connect").await?
        {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        ctx.human_pause(400, 800).await?;
    }
}

/// Poll until an enabled `button` containing `text` exists, or `timeout`.
async fn wait_button_enabled(ctx: &WorkflowCtx, text: &str, timeout: Duration) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        ctx.guard().await?;
        if util::enabled_contains(ctx, "button", text).await? {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        ctx.human_pause(400, 800).await?;
    }
}

/// A long wait that stays responsive to STOP — sleeps in ~1s chunks, checking the
/// guard between them, instead of one uninterruptible `human_pause`. Used for the
/// slow cold-boot Windows 11 steps (Start menu populating, search resolving,
/// Terminal launching), which can each take 20-30s.
async fn cold_wait(ctx: &WorkflowCtx, min_ms: u64, max_ms: u64) -> Result<()> {
    let mut left = rand::rng().random_range(min_ms..=max_ms);
    while left > 0 {
        ctx.guard().await?;
        let chunk = left.min(1000);
        tokio::time::sleep(Duration::from_millis(chunk)).await;
        left -= chunk;
    }
    ctx.guard().await
}

/// Type `s` into the VM stream as PHYSICAL key events (scancode + shift), which a
/// remote-desktop stream actually forwards (the JS `text`-based path is dropped).
/// Human-paced.
async fn type_str(ctx: &WorkflowCtx, s: &str) -> Result<()> {
    for c in s.chars() {
        ctx.guard().await?;
        ctx.send_char_vm(c).await?;
        ctx.human_pause(55, 150).await?;
    }
    Ok(())
}

/// `/tasks/<id>/` extractor (shared shape with submit_fill).
fn task_id_from_url(url: &str) -> Option<String> {
    let after = url.split("/tasks/").nth(1)?;
    let id = after.split('/').next().unwrap_or("");
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

/// True once the Vagon VM stream is live. The boot/connect overlay is a
/// `.vg-loader` spinner (with a "Turning On"/"Connecting..." `.description`) that's
/// present the whole time the VM is coming up and removed/hidden once the desktop
/// is streaming — so its absence is the reliable "connected" signal (the `#player`
/// video's own CSS `display` is unreliable to key on). We also accept the video
/// having real frame data (`videoWidth>0`) as a positive confirmation.
const PLAYER_READY_JS: &str = r#"(function(){
  var loader = document.querySelector('.vg-loader');
  if (loader && loader.offsetParent !== null) return false; // still Turning On / Connecting
  // Spinner gone -> the desktop is up. Confirm a #player video element exists.
  return !!document.querySelector('#player');
})()"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_id_and_prefix() {
        let url = "https://feather.openai.com/tasks/d3bb5691-2ad7-44f4-8aa9-1298ead0c2fc/stage/execution";
        assert_eq!(task_id_from_url(url).as_deref(), Some("d3bb5691-2ad7-44f4-8aa9-1298ead0c2fc"));
        // The folder prefix is taken from the first UUID segment.
        let seg = task_id_from_url(url).unwrap_or_default();
        let first = seg.split('-').next().unwrap_or_default();
        assert_eq!(first, "d3bb5691");
        assert!(first.len() == 8);
    }

    #[test]
    fn cir_name_suffix() {
        let mk = |s: &str| {
            if s.trim().to_ascii_lowercase().ends_with(".cir") {
                s.trim().to_string()
            } else {
                format!("{}.cir", s.trim())
            }
        };
        assert_eq!(mk("LowPassFilter"), "LowPassFilter.cir");
        assert_eq!(mk("LowPassFilter.cir"), "LowPassFilter.cir");
        assert_eq!(mk("  roc  "), "roc.cir");
    }
}
