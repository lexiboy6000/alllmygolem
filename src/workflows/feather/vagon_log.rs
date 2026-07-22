//! "Create Vagon log" — on an already-loaded Vagon session, publish a User
//! Activity Log entry (with the default task-folder snapshot): click the Vagon
//! logo to expand the dock, open Recordings, choose "Add User Activity Log", type
//! what changed, and Send.
//!
//! Unlike the VM desktop (a pixel stream), the Vagon dock + modal are real DOM, so
//! this is selector/text-based clicking. It attaches to the existing Vagon tab
//! (it does NOT navigate — that would reload the live session).

use std::time::Duration;

use crate::prelude::*;

use super::util;

pub struct CreateVagonLog;

#[async_trait]
impl Workflow for CreateVagonLog {
    fn name(&self) -> &'static str {
        "Create Vagon log"
    }
    fn description(&self) -> &'static str {
        "On a loaded Vagon session, publish a User Activity Log entry (+ folder snapshot)."
    }
    fn inputs(&self) -> Vec<InputSpec> {
        vec![
            InputSpec::required("vagon_url", "Vagon session URL (https://app.vagon.io/team/session/...)"),
            InputSpec::required("log_text", "What changed (the activity log message)"),
        ]
    }

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        let wait = Duration::from_millis(ctx.settings.default_wait_timeout_ms);

        let vagon_url = ctx.require_input("vagon_url")?;
        if !vagon_url.contains("app.vagon.io") {
            return Err(ctx
                .stop_and_warn(format!(
                    "The URL must be a Vagon session URL (https://app.vagon.io/team/session/...): {vagon_url}"
                ))
                .await);
        }
        let log_text = ctx.require_input("log_text")?;
        if log_text.trim().is_empty() {
            return Err(ctx.stop_and_warn("The activity log message is empty.").await);
        }
        let needle = util::vagon_session_needle(&vagon_url);

        // --- attach to the (already-loaded) Vagon tab ---
        ctx.step("attach to Vagon session").await?;
        if !ctx.switch_to_target(&needle, "expired", wait.max(Duration::from_secs(30))).await? {
            return Err(ctx
                .stop_and_warn(
                    "Could not find the loaded Vagon session tab. Open the session first (Vagon \
                     must be loaded) and try again.",
                )
                .await);
        }
        // --- publish the activity log on the attached tab ---
        ctx.step("publish activity log").await?;
        if let Err(e) = publish_log(ctx, &log_text).await {
            return Err(ctx
                .stop_and_warn(format!("Could not publish the Vagon activity log: {e}"))
                .await);
        }

        ctx.warn_user(format!(
            "Published Vagon activity log: \"{}\". (The Recordings dock stays expanded, so another \
             log can be added without re-opening it.)",
            log_text.trim()
        ))
        .await?;

        Ok(WorkflowOutcome::CompletedWith(json!({
            "vagon_url": vagon_url,
            "log_text": log_text,
        })))
    }
}

/// Publish a User Activity Log on the CURRENTLY-attached Vagon tab: expand the
/// dock (if collapsed), open Recordings → "Add User Activity Log", type the
/// message, Send, and wait for the modal to dismiss. The caller must already be
/// on the session tab (e.g. via `switch_to_target`, or because it's typing into
/// that tab's VM stream). Returns a plain Halted error (NO user prompt) on
/// failure so callers — like the VM execute workflow that logs at checkpoints —
/// can warn-and-continue rather than block.
pub async fn publish_log(ctx: &mut WorkflowCtx, log_text: &str) -> Result<()> {
    let wait = Duration::from_millis(ctx.settings.default_wait_timeout_ms);

    if !wait_eval(ctx, LOGO_PRESENT_JS, wait).await? {
        return Err(ctx.halt("the Vagon control dock (logo) isn't on the page — is the session loaded?"));
    }
    // Expand the dock only if it isn't already (clicking the logo when open closes it).
    ctx.note_status("opening the Vagon dock");
    if !ctx.eval(BAR_VISIBLE_JS).await?.as_bool().unwrap_or(false) {
        if !util::click_visible(ctx, ".fd-collapsed-icon", wait).await? {
            return Err(ctx.halt("could not click the Vagon logo to open the dock"));
        }
        if !wait_eval(ctx, BAR_VISIBLE_JS, wait).await? {
            return Err(ctx.halt("the dock bar didn't expand after clicking the Vagon logo"));
        }
        ctx.human_pause(400, 900).await?;
    }

    ctx.note_status("opening Recordings");
    if !util::click_visible(ctx, "button[aria-label=\"Recordings\"]", wait).await? {
        return Err(ctx.halt("could not click the Recordings button in the dock"));
    }
    ctx.human_pause(400, 900).await?;
    if !util::wait_for_text(ctx, "button.fd-menu-item", "Add User Activity Log", wait).await? {
        return Err(ctx.halt("the 'Add User Activity Log' menu item didn't appear"));
    }
    if !util::click_contains(ctx, "button.fd-menu-item", "Add User Activity Log").await? {
        return Err(ctx.halt("could not click 'Add User Activity Log'"));
    }

    ctx.note_status("opening the activity-log modal");
    if !wait_eval(ctx, MODAL_INPUT_JS, wait).await? {
        return Err(ctx.halt("the User Activity Log modal (input) didn't open"));
    }
    ctx.human_pause(400, 900).await?;
    // Fill the input via React's native value setter + an input event — NOT
    // keystrokes. The Vagon stream captures the keyboard globally, so typed keys
    // can miss the modal (and even leak into the VM); setting the value directly
    // is reliable and is what flips Send from disabled to enabled.
    ctx.note_status("writing the activity-log message");
    let set_js = REACT_SET_INPUT_JS.replace("__TXT__", &util::js_str(log_text));
    if !ctx.eval(&set_js).await?.as_bool().unwrap_or(false) {
        return Err(ctx.halt("could not fill the activity-log input"));
    }
    ctx.human_pause(400, 900).await?;

    // Send enables once the message registers AND Vagon finishes its snapshot,
    // which can take several seconds — wait generously (bounded).
    ctx.note_status("waiting for Send (Vagon may be capturing a snapshot)…");
    if !wait_eval(ctx, SEND_ENABLED_JS, wait.max(Duration::from_secs(45))).await? {
        return Err(ctx.halt(
            "the Send button stayed disabled (the message didn't register, or the snapshot is still pending)",
        ));
    }
    ctx.note_status("sending the activity log");
    if !util::click_visible(ctx, ".record-log-submit-button", wait).await? {
        return Err(ctx.halt("could not click the Send button"));
    }
    ctx.note_status("waiting for the log to post");
    if !wait_eval(ctx, MODAL_GONE_JS, wait.max(Duration::from_secs(30))).await? {
        return Err(ctx.halt("the log modal didn't dismiss after Send"));
    }
    Ok(())
}

/// Poll `js` (must return a boolean) until it's `true`, or `timeout`.
async fn wait_eval(ctx: &WorkflowCtx, js: &str, timeout: Duration) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        ctx.guard().await?;
        if ctx.eval(js).await?.as_bool().unwrap_or(false) {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        ctx.human_pause(250, 500).await?;
    }
}

// --- JS evaluators ---

const LOGO_PRESENT_JS: &str = r#"!!document.querySelector('.fd-vagon-logo, .fd-collapsed-icon')"#;

const BAR_VISIBLE_JS: &str = r#"(function(){
  var b = document.querySelector('.fd-bar-wrap');
  if (!b) return false;
  return getComputedStyle(b).visibility !== 'hidden';
})()"#;

const MODAL_INPUT_JS: &str = r#"!!document.querySelector('.record-log-modal input')"#;

// Set the modal input's value via React's native setter + input/change events, so
// a controlled React input registers the message and enables Send — without
// sending any keystrokes (which the Vagon stream can intercept). `__TXT__` is
// replaced with a JS string literal.
const REACT_SET_INPUT_JS: &str = r#"(function(){
  var i = document.querySelector('.record-log-modal input, .record-log-modal textarea');
  if (!i) return false;
  var proto = i.tagName === 'TEXTAREA' ? window.HTMLTextAreaElement.prototype : window.HTMLInputElement.prototype;
  var d = Object.getOwnPropertyDescriptor(proto, 'value');
  try { i.focus(); } catch(e){}
  if (d && d.set) { d.set.call(i, __TXT__); } else { i.value = __TXT__; }
  i.dispatchEvent(new Event('input', { bubbles: true }));
  i.dispatchEvent(new Event('change', { bubbles: true }));
  return true;
})()"#;

const SEND_ENABLED_JS: &str = r#"(function(){
  var s = document.querySelector('.record-log-submit-button');
  return !!(s && !s.disabled && getComputedStyle(s).display !== 'none');
})()"#;

const MODAL_GONE_JS: &str = r#"!document.querySelector('.record-log-modal')"#;
