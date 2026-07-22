//! Small DOM helpers shared by the feather workflows. They lean on `ctx.eval`
//! (arbitrary JS) for things CSS selectors can't express (matching by visible
//! text), and on `ctx.click`/`ctx.click_at` for human-like clicking.

use crate::prelude::*;

/// Quote a Rust string as a safe JS string literal (e.g. `a` -> `"a"`).
pub fn js_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// Evaluate a JS *body* (which may use `return`) wrapped in an IIFE.
pub async fn eval_fn(ctx: &WorkflowCtx, body: &str) -> Result<Value> {
    ctx.eval(&format!("(function() {{ {body} }})()")).await
}

pub async fn hostname(ctx: &WorkflowCtx) -> Result<String> {
    Ok(ctx
        .eval("location.hostname")
        .await?
        .as_str()
        .unwrap_or_default()
        .to_string())
}

pub async fn pathname(ctx: &WorkflowCtx) -> Result<String> {
    Ok(ctx
        .eval("location.pathname")
        .await?
        .as_str()
        .unwrap_or_default()
        .to_string())
}

pub async fn href(ctx: &WorkflowCtx) -> Result<String> {
    Ok(ctx
        .eval("location.href")
        .await?
        .as_str()
        .unwrap_or_default()
        .to_string())
}

/// True if any element matching `selector` has trimmed text exactly `text`.
pub async fn exists_with_text(ctx: &WorkflowCtx, selector: &str, text: &str) -> Result<bool> {
    let body = TEXT_EXISTS_JS
        .replace("__SEL__", &js_str(selector))
        .replace("__TXT__", &js_str(text));
    Ok(eval_fn(ctx, &body).await?.as_bool().unwrap_or(false))
}

/// Find the first element matching `selector` whose trimmed text == `text`,
/// scroll it into view, and human-click its centre. `Ok(true)` if it was found
/// and clicked, `Ok(false)` if no such element exists.
pub async fn click_text(ctx: &mut WorkflowCtx, selector: &str, text: &str) -> Result<bool> {
    let body = TEXT_CENTER_JS
        .replace("__SEL__", &js_str(selector))
        .replace("__TXT__", &js_str(text));
    let v = eval_fn(ctx, &body).await?;
    if v.is_null() {
        return Ok(false);
    }
    match (
        v.get("x").and_then(Value::as_f64),
        v.get("y").and_then(Value::as_f64),
    ) {
        (Some(x), Some(y)) => {
            ctx.click_at(x, y).await?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// True if any element matching `selector` has trimmed text *containing*
/// `text`. Lenient (substring) to tolerate nested nodes / surrounding markup.
pub async fn contains_text(ctx: &WorkflowCtx, selector: &str, text: &str) -> Result<bool> {
    let body = TEXT_CONTAINS_JS
        .replace("__SEL__", &js_str(selector))
        .replace("__TXT__", &js_str(text));
    Ok(eval_fn(ctx, &body).await?.as_bool().unwrap_or(false))
}

/// Poll until an element matching `selector` contains `text`, or `timeout`
/// elapses. SPA pages often render tabs/content a beat after navigation, so a
/// one-shot check races the render; this waits it out. Cancellable via guard.
pub async fn wait_for_text(
    ctx: &WorkflowCtx,
    selector: &str,
    text: &str,
    timeout: Duration,
) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        ctx.guard().await?;
        if contains_text(ctx, selector, text).await? {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        ctx.human_pause(150, 300).await?;
    }
}

/// Like [`click_text`] but matches the first element whose trimmed text
/// *contains* `text` (lenient — good for tab labels that may have extra markup).
pub async fn click_contains(ctx: &mut WorkflowCtx, selector: &str, text: &str) -> Result<bool> {
    let body = TEXT_CONTAINS_CENTER_JS
        .replace("__SEL__", &js_str(selector))
        .replace("__TXT__", &js_str(text));
    let v = eval_fn(ctx, &body).await?;
    if v.is_null() {
        return Ok(false);
    }
    match (
        v.get("x").and_then(Value::as_f64),
        v.get("y").and_then(Value::as_f64),
    ) {
        (Some(x), Some(y)) => {
            ctx.click_at(x, y).await?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Like [`click_contains`] but only clicks a button that is ENABLED (not
/// `disabled` / `aria-disabled`, and actually laid out). Returns `Ok(false)`
/// when the only matches are greyed-out — so callers can poll until a control
/// becomes live (e.g. Vagon "Connect", which is disabled until the VM is ready).
pub async fn click_enabled_contains(
    ctx: &mut WorkflowCtx,
    selector: &str,
    text: &str,
) -> Result<bool> {
    let body = TEXT_CONTAINS_ENABLED_CENTER_JS
        .replace("__SEL__", &js_str(selector))
        .replace("__TXT__", &js_str(text));
    let v = eval_fn(ctx, &body).await?;
    if v.is_null() {
        return Ok(false);
    }
    match (
        v.get("x").and_then(Value::as_f64),
        v.get("y").and_then(Value::as_f64),
    ) {
        (Some(x), Some(y)) => {
            ctx.click_at(x, y).await?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Whether at least one `selector` element whose text contains `text` is ENABLED
/// (not `disabled`/`aria-disabled`, and laid out). Lets a workflow read button
/// state (e.g. is "Set task" still clickable, has "Connect" come alive?) without
/// clicking it.
pub async fn enabled_contains(ctx: &WorkflowCtx, selector: &str, text: &str) -> Result<bool> {
    let body = TEXT_CONTAINS_ENABLED_EXISTS_JS
        .replace("__SEL__", &js_str(selector))
        .replace("__TXT__", &js_str(text));
    Ok(eval_fn(ctx, &body).await?.as_bool().unwrap_or(false))
}

/// Read the Vagon "computer" Status value — the bold span under the "Status"
/// label on the feather Task-execution tab (e.g. `ready`, `turning_on`,
/// `turning_off`). `None` if that card isn't on the page.
pub async fn vagon_status(ctx: &WorkflowCtx) -> Result<Option<String>> {
    let v = ctx.eval(VAGON_STATUS_JS).await?;
    Ok(v.as_str()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty()))
}

/// Click the Vagon card's Refresh icon to re-fetch the VM status (the status
/// text does NOT live-update — it only changes when refreshed). `Ok(false)` if
/// the refresh button isn't present.
pub async fn refresh_vagon_status(ctx: &mut WorkflowCtx) -> Result<bool> {
    let v = ctx.eval(VAGON_REFRESH_JS).await?;
    match (
        v.get("x").and_then(Value::as_f64),
        v.get("y").and_then(Value::as_f64),
    ) {
        (Some(x), Some(y)) => {
            ctx.human_pause(200, 500).await?;
            ctx.click_at(x, y).await?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// The Vagon "quick connect" login link (`<a href="https://app.vagon.io/team/
/// session/...">`) on the feather card. We open the VM via THIS anchor rather
/// than the Connect button, whose popup Chromium blocks. `None` if absent.
pub async fn vagon_login_link(ctx: &WorkflowCtx) -> Result<Option<String>> {
    let v = ctx.eval(VAGON_LOGIN_LINK_JS).await?;
    Ok(v.as_str()
        .map(|s| s.trim().to_string())
        .filter(|s| s.contains("app.vagon.io")))
}

/// Wait for the first VISIBLE, enabled element matching `selector` and human-
/// click its centre. `Ok(false)` if none became visible within `timeout`.
pub async fn click_visible(ctx: &mut WorkflowCtx, selector: &str, timeout: Duration) -> Result<bool> {
    let js = FIND_VISIBLE_JS.replace("__SEL__", &js_str(selector));
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        ctx.guard().await?;
        let v = ctx.eval(&js).await?;
        if let (Some(x), Some(y)) = (
            v.get("x").and_then(Value::as_f64),
            v.get("y").and_then(Value::as_f64),
        ) {
            ctx.human_pause(250, 600).await?; // aim
            ctx.click_at(x, y).await?;
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        ctx.human_pause(250, 500).await?;
    }
}

/// `https://app.vagon.io/team/session/<id>` -> a `session/<id>` needle so callers
/// attach to THIS session's tab (falls back to the host for an unusual shape, and
/// never keys off the `expired` zombie tab).
pub fn vagon_session_needle(url: &str) -> String {
    if let Some(after) = url.split("/session/").nth(1) {
        let id = after.split(['/', '?', '#']).next().unwrap_or("");
        if !id.is_empty() && id != "expired" {
            return format!("session/{id}");
        }
    }
    "app.vagon.io".to_string()
}

/// Confirm the browser is on `feather.openai.com` at the given path (trailing
/// `?`, `&`, `/` and query strings are tolerated, per the spec).
pub async fn is_on(ctx: &WorkflowCtx, expected_path: &str) -> Result<bool> {
    let host = hostname(ctx).await?;
    if host != "feather.openai.com" {
        return Ok(false);
    }
    let path = pathname(ctx).await?;
    let norm = |p: &str| p.trim_end_matches('/').to_string();
    let want = norm(expected_path);
    let got = norm(&path);
    // Treat "/" and "" as equivalent for the homepage.
    Ok(got == want || (want.is_empty() && got.is_empty()))
}

/// Poll until the browser is on `expected_path` (feather.openai.com), or
/// `timeout` elapses. SPA navigations update the URL a beat after the click, so
/// a one-shot check races; this waits it out. Cancellable via guard.
pub async fn wait_until_on(
    ctx: &WorkflowCtx,
    expected_path: &str,
    timeout: Duration,
) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        ctx.guard().await?;
        if is_on(ctx, expected_path).await? {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        ctx.human_pause(150, 300).await?;
    }
}

/// Poll until `location.href` contains `needle`, or `timeout` elapses.
pub async fn wait_until_href_contains(
    ctx: &WorkflowCtx,
    needle: &str,
    timeout: Duration,
) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        ctx.guard().await?;
        if href(ctx).await?.contains(needle) {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        ctx.human_pause(150, 300).await?;
    }
}

const TEXT_EXISTS_JS: &str = r#"
var els = document.querySelectorAll(__SEL__);
for (var i = 0; i < els.length; i++) {
    if ((els[i].textContent || '').trim() === __TXT__) return true;
}
return false;
"#;

const TEXT_CENTER_JS: &str = r#"
var els = document.querySelectorAll(__SEL__);
for (var i = 0; i < els.length; i++) {
    var e = els[i];
    if ((e.textContent || '').trim() === __TXT__) {
        e.scrollIntoView({ block: 'center', inline: 'center' });
        var r = e.getBoundingClientRect();
        return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
    }
}
return null;
"#;

const TEXT_CONTAINS_JS: &str = r#"
var els = document.querySelectorAll(__SEL__);
for (var i = 0; i < els.length; i++) {
    if ((els[i].textContent || '').trim().indexOf(__TXT__) !== -1) return true;
}
return false;
"#;

const TEXT_CONTAINS_CENTER_JS: &str = r#"
var els = document.querySelectorAll(__SEL__);
for (var i = 0; i < els.length; i++) {
    var e = els[i];
    if ((e.textContent || '').trim().indexOf(__TXT__) !== -1) {
        e.scrollIntoView({ block: 'center', inline: 'center' });
        var r = e.getBoundingClientRect();
        return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
    }
}
return null;
"#;

// First ENABLED, laid-out element matching SEL whose text contains TXT.
const TEXT_CONTAINS_ENABLED_CENTER_JS: &str = r#"
var els = document.querySelectorAll(__SEL__);
for (var i = 0; i < els.length; i++) {
    var e = els[i];
    if ((e.textContent || '').trim().indexOf(__TXT__) === -1) continue;
    if (e.disabled || e.getAttribute('aria-disabled') === 'true') continue;
    if (e.offsetParent === null) continue;
    e.scrollIntoView({ block: 'center', inline: 'center' });
    var r = e.getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
}
return null;
"#;

// True if any ENABLED, laid-out element matching SEL has text containing TXT.
const TEXT_CONTAINS_ENABLED_EXISTS_JS: &str = r#"
var els = document.querySelectorAll(__SEL__);
for (var i = 0; i < els.length; i++) {
    var e = els[i];
    if ((e.textContent || '').trim().indexOf(__TXT__) === -1) continue;
    if (e.disabled || e.getAttribute('aria-disabled') === 'true') continue;
    if (e.offsetParent === null) continue;
    return true;
}
return false;
"#;

// Centre of the first VISIBLE, enabled element matching SEL (IIFE — eval directly).
const FIND_VISIBLE_JS: &str = r#"(function(){
  var e = document.querySelector(__SEL__);
  if (!e || e.offsetParent === null) return null;
  var s = getComputedStyle(e);
  if (s.visibility === 'hidden' || s.display === 'none' || e.disabled) return null;
  try { e.scrollIntoView({ block:'center', inline:'center' }); } catch(x){}
  var r = e.getBoundingClientRect();
  if (r.width < 1 || r.height < 1) return null;
  return { x: r.left + r.width/2, y: r.top + r.height/2 };
})()"#;

// The Vagon "computer" status value (the bold span under the "Status" label).
const VAGON_STATUS_JS: &str = r#"(function(){
  var ls = document.querySelectorAll('span.text-xs.uppercase');
  for (var i=0;i<ls.length;i++){
    if ((ls[i].textContent||'').trim().toLowerCase()==='status'){
      var p = ls[i].parentElement;
      if (p){ var v = p.querySelector('span.font-semibold'); if (v) return (v.textContent||'').trim(); }
    }
  }
  return null;
})()"#;

// Centre of the Vagon card's Refresh icon button.
const VAGON_REFRESH_JS: &str = r#"(function(){
  var icon = document.querySelector('svg[data-testid="RefreshIcon"]');
  if (!icon) return null;
  var btn = icon.closest('button');
  if (!btn || btn.offsetParent === null || btn.disabled) return null;
  try { btn.scrollIntoView({ block:'center', inline:'center' }); } catch(e){}
  var r = btn.getBoundingClientRect();
  return { x: r.left + r.width/2, y: r.top + r.height/2 };
})()"#;

// The Vagon "quick connect" login-link href.
const VAGON_LOGIN_LINK_JS: &str = r#"(function(){
  var a = document.querySelector('a[href*="app.vagon.io/team/session"]');
  return a ? a.href : null;
})()"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_needle() {
        assert_eq!(
            vagon_session_needle("https://app.vagon.io/team/session/4d6e25c8-b192-4ef9-ba84-8973eb5f1f11"),
            "session/4d6e25c8-b192-4ef9-ba84-8973eb5f1f11"
        );
        // Query/hash are trimmed.
        assert_eq!(
            vagon_session_needle("https://app.vagon.io/team/session/abc123?x=1"),
            "session/abc123"
        );
        // Unusual shape -> fall back to host.
        assert_eq!(vagon_session_needle("https://app.vagon.io/team"), "app.vagon.io");
        // Never key off the expired zombie tab.
        assert_eq!(vagon_session_needle("https://app.vagon.io/team/session/expired"), "app.vagon.io");
    }
}
