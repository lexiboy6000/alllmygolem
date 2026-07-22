//! CDP backend: attaches to a running Chrome over the DevTools Protocol,
//! controls a target, auto-reconnects with backoff, and (optionally) relaunches
//! Chrome if it dies. Implements [`BrowserBackend`].
//!
//! Design notes (see `docs/STABILITY.md`):
//! - Nothing here can panic. Every fallible operation returns
//!   [`crate::error::Result`]; foreign errors are mapped with `.map_err`.
//! - DOM access prefers `page.evaluate(...)` with the selector injected as a
//!   JSON string literal (so a selector can never break out of the script).
//! - Idempotent reads retry once on a transient CDP/connection error.
//! - A supervisor task drives the chromiumoxide [`Handler`] stream; when it
//!   ends (the socket dropped) it reconnects with exponential backoff and,
//!   optionally, relaunches Chrome.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use parking_lot::RwLock;
use serde_json::Value;
use tokio::sync::oneshot;

use chromiumoxide::cdp::browser_protocol::browser::{
    SetDownloadBehaviorBehavior, SetDownloadBehaviorParams,
};
use chromiumoxide::cdp::browser_protocol::emulation::SetFocusEmulationEnabledParams;
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams, DispatchMouseEventType,
    MouseButton as CdpMouseButton,
};
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide::{Browser, Command, Page};

use crate::backend::{BrowserBackend, MouseButton};
use crate::error::{GolemError, Result};
use crate::geometry::Rect;
use crate::messages::{ConnState, EngineEvent, EventTx};
use crate::settings::Settings;

/// Everything needed to attach to (and optionally relaunch) Chrome.
#[derive(Clone, Debug)]
pub struct ConnectionConfig {
    pub host: String,
    pub port: u16,
    pub chrome_path: Option<String>,
    pub user_data_dir: Option<String>,
    pub auto_relaunch: bool,
    pub download_dir: PathBuf,
    pub call_timeout: Duration,
    pub reconnect_initial: Duration,
    pub reconnect_max: Duration,
    pub reconnect_max_attempts: u32,
}

impl ConnectionConfig {
    pub fn from_settings(s: &Settings) -> Self {
        ConnectionConfig {
            host: s.chrome_host.clone(),
            port: s.chrome_port,
            chrome_path: s.chrome_path.clone(),
            user_data_dir: s.chrome_user_data_dir.clone(),
            auto_relaunch: s.auto_relaunch_chrome,
            download_dir: s.download_dir(),
            call_timeout: Duration::from_millis(s.cdp_call_timeout_ms),
            reconnect_initial: Duration::from_millis(s.reconnect_initial_ms),
            reconnect_max: Duration::from_millis(s.reconnect_max_ms),
            reconnect_max_attempts: s.reconnect_max_attempts,
        }
    }

    pub fn devtools_http(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }
}

/// CDP-backed browser handle.
///
/// The currently controlled [`Page`] lives behind an `RwLock` so the reconnect
/// supervisor can swap it without disturbing in-flight workflow code.
pub struct CdpBrowser {
    config: ConnectionConfig,
    events: EventTx,
    page: RwLock<Option<Arc<Page>>>,
    /// The browser handle, shared with the reconnect supervisor, so we can
    /// enumerate targets (e.g. switch to a popup tab the page opened). `None`
    /// while reconnecting.
    browser: RwLock<Option<Arc<Browser>>>,
}

impl CdpBrowser {
    /// Attach to Chrome, pick a target, and spawn the handler + reconnect
    /// supervisor tasks. Returns once the first attach succeeds.
    pub async fn connect(config: ConnectionConfig, events: EventTx) -> Result<Arc<CdpBrowser>> {
        let _ = events.send(EngineEvent::Connection(ConnState::Connecting));

        let (browser, page, disc_rx) = establish_with_timeout(&config).await?;
        let target_url = page_url_opt(&page, config.call_timeout).await;

        let cdp = Arc::new(CdpBrowser {
            config: config.clone(),
            events: events.clone(),
            page: RwLock::new(Some(page)),
            browser: RwLock::new(Some(Arc::new(browser))),
        });

        let _ = events.send(EngineEvent::Connection(ConnState::Connected { target_url }));
        tracing::info!("attached to chrome at {}", config.devtools_http());

        // The supervisor keeps the connection alive (via the shared browser handle
        // on `cdp`) and watches the disconnect signal so it can rebuild on a drop.
        let sup = cdp.clone();
        tokio::spawn(async move {
            supervise(sup, disc_rx).await;
        });

        Ok(cdp)
    }

    /// Clone the currently controlled page, or error if none is attached.
    fn page(&self) -> Result<Arc<Page>> {
        self.page
            .read()
            .clone()
            .ok_or_else(|| GolemError::Connection("no target".into()))
    }

    /// Run `f` against a live page, re-acquiring the page and retrying on
    /// connection-type failures. When the CDP handler dies (Chrome dropped the
    /// debug socket), the supervisor races to reconnect and swap in a fresh
    /// page; this rides out that window instead of surfacing "receiver is gone".
    async fn with_page<T, F>(&self, what: &str, f: F) -> Result<T>
    where
        F: AsyncFn(Arc<Page>) -> Result<T>,
    {
        // Patient enough to ride out the supervisor's reconnect (first attempt at
        // ~500ms; a healthy Chrome is usually back within a couple seconds). Total
        // window ~17s so a transient WS blip / brief renderer hiccup recovers; a
        // genuinely dead Chrome still fails after that with a clear message.
        const TRIES: u32 = 10;
        let mut delay = Duration::from_millis(250);
        let mut last: Option<GolemError> = None;
        for i in 0..TRIES {
            match self.page() {
                Ok(page) => match f(page).await {
                    Ok(v) => return Ok(v),
                    // Retry only connection-type errors; surface real failures.
                    Err(e) if is_conn_err(&e) => last = Some(e),
                    Err(e) => return Err(e),
                },
                Err(e) => last = Some(e), // no target yet (mid-reconnect)
            }
            if i + 1 < TRIES {
                tokio::time::sleep(delay).await;
                delay = delay
                    .checked_mul(2)
                    .unwrap_or(delay)
                    .min(Duration::from_millis(2500));
            }
        }
        // Exhausted: if the cause was a lost connection, say so plainly instead of
        // surfacing the raw "send failed because receiver is gone".
        Err(match last {
            Some(e) if is_conn_err(&e) => GolemError::Connection(format!(
                "{what}: lost the connection to Chrome (the tab may have crashed — check for \
                 \"Aw, snap!\" — or Chrome was closed). Relaunch Chrome and reconnect."
            )),
            Some(e) => e,
            None => GolemError::Connection(format!("{what}: connection unavailable")),
        })
    }

    /// Run a single CDP command (retried across reconnects).
    async fn execute_cmd<C>(&self, cmd: C) -> Result<()>
    where
        C: Command + Clone,
    {
        let timeout = self.config.call_timeout;
        self.with_page("cdp command", async move |page: Arc<Page>| {
            match tokio::time::timeout(timeout, page.execute(cmd.clone())).await {
                Err(_) => Err(GolemError::Timeout("cdp command".into())),
                Ok(Ok(_)) => Ok(()),
                Ok(Err(e)) => Err(GolemError::Browser(format!("cdp command: {e}"))),
            }
        })
        .await
    }

    /// Evaluate JS and return its JSON value (`undefined` → [`Value::Null`]),
    /// retried across reconnects.
    async fn eval_value(&self, js: &str) -> Result<Value> {
        let timeout = self.config.call_timeout;
        let js = js.to_string();
        self.with_page("eval", async move |page: Arc<Page>| {
            match tokio::time::timeout(timeout, page.evaluate(js.as_str())).await {
                Err(_) => Err(GolemError::Timeout("eval".into())),
                Ok(Ok(res)) => Ok(res.value().cloned().unwrap_or(Value::Null)),
                Ok(Err(e)) => Err(GolemError::Browser(format!("eval: {e}"))),
            }
        })
        .await
    }
}

/// Whether an error means the connection/handler is gone and the operation
/// should be retried after the supervisor reconnects.
fn is_conn_err(e: &GolemError) -> bool {
    match e {
        GolemError::Connection(_) => true,
        GolemError::Browser(m) => {
            let m = m.to_ascii_lowercase();
            m.contains("receiver is gone")
                || m.contains("channel")
                || m.contains("connection")
                || m.contains("closed")
                || m.contains("websocket")
                || m.contains("no target")
        }
        _ => false,
    }
}

#[async_trait]
impl BrowserBackend for CdpBrowser {
    async fn navigate(&self, url: &str) -> Result<()> {
        let timeout = self.config.call_timeout;
        let url = url.to_string();
        self.with_page("navigate", async move |page: Arc<Page>| {
            let work = async {
                page.goto(url.as_str())
                    .await
                    .map_err(|e| GolemError::Browser(format!("goto {url}: {e}")))?;
                page.wait_for_navigation()
                    .await
                    .map_err(|e| GolemError::Browser(format!("wait navigation: {e}")))?;
                Ok::<(), GolemError>(())
            };
            match tokio::time::timeout(timeout, work).await {
                Err(_) => Err(GolemError::Timeout(format!("navigate {url}"))),
                Ok(r) => r,
            }
        })
        .await
    }

    async fn current_url(&self) -> Result<String> {
        let timeout = self.config.call_timeout;
        self.with_page("current_url", async move |page: Arc<Page>| {
            match tokio::time::timeout(timeout, page.url()).await {
                Err(_) => Err(GolemError::Timeout("current_url".into())),
                Ok(Ok(opt)) => Ok(opt.unwrap_or_default()),
                Ok(Err(e)) => Err(GolemError::Browser(format!("url: {e}"))),
            }
        })
        .await
    }

    async fn wait_for_selector(&self, selector: &str, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.query_exists(selector).await? {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(GolemError::Timeout(format!(
                    "selector not found within {timeout:?}: {selector}"
                )));
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    async fn query_exists(&self, selector: &str) -> Result<bool> {
        let s = serde_json::to_string(selector)?;
        let js = format!("!!document.querySelector({s})");
        let v = self.eval_value(&js).await?;
        Ok(v.as_bool().unwrap_or(false))
    }

    async fn get_attribute(&self, selector: &str, name: &str) -> Result<Option<String>> {
        let s = serde_json::to_string(selector)?;
        let n = serde_json::to_string(name)?;
        let js = format!(
            "(function(){{var el=document.querySelector({s});if(!el)return null;var v=el.getAttribute({n});return v===null?null:v;}})()"
        );
        let v = self.eval_value(&js).await?;
        Ok(match v {
            Value::Null => None,
            Value::String(s) => Some(s),
            other => Some(other.to_string()),
        })
    }

    async fn get_text(&self, selector: &str) -> Result<Option<String>> {
        let s = serde_json::to_string(selector)?;
        let js = format!(
            "(function(){{var el=document.querySelector({s});if(!el)return null;return (el.innerText!==undefined&&el.innerText!==null)?el.innerText:(el.textContent||\"\");}})()"
        );
        let v = self.eval_value(&js).await?;
        Ok(match v {
            Value::Null => None,
            Value::String(s) => Some(s),
            other => Some(other.to_string()),
        })
    }

    async fn get_rect(&self, selector: &str) -> Result<Option<Rect>> {
        let s = serde_json::to_string(selector)?;
        let js = format!(
            "(function(){{var el=document.querySelector({s});if(!el)return null;if(typeof el.scrollIntoViewIfNeeded==='function'){{el.scrollIntoViewIfNeeded(true);}}else{{el.scrollIntoView({{block:'center',inline:'center'}});}}var r=el.getBoundingClientRect();return {{x:r.x,y:r.y,width:r.width,height:r.height}};}})()"
        );
        let v = self.eval_value(&js).await?;
        if v.is_null() {
            return Ok(None);
        }
        let rect: Rect = serde_json::from_value(v)
            .map_err(|e| GolemError::Browser(format!("parse rect: {e}")))?;
        Ok(Some(rect))
    }

    async fn eval(&self, js: &str) -> Result<Value> {
        self.eval_value(js).await
    }

    async fn focus(&self, selector: &str) -> Result<()> {
        let s = serde_json::to_string(selector)?;
        let js = format!("document.querySelector({s})?.focus()");
        self.eval_value(&js).await?;
        Ok(())
    }

    async fn mouse_move(&self, x: f64, y: f64) -> Result<()> {
        let params = DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MouseMoved)
            .x(x)
            .y(y)
            .buttons(0_i64)
            .build()
            .map_err(|e| GolemError::Browser(format!("mouse_move params: {e}")))?;
        self.execute_cmd(params).await
    }

    async fn mouse_press(&self, button: MouseButton, x: f64, y: f64) -> Result<()> {
        let (btn, bits) = map_button(button);
        let params = DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MousePressed)
            .x(x)
            .y(y)
            .button(btn)
            .buttons(bits)
            .click_count(1_i64)
            .build()
            .map_err(|e| GolemError::Browser(format!("mouse_press params: {e}")))?;
        self.execute_cmd(params).await
    }

    async fn mouse_release(&self, button: MouseButton, x: f64, y: f64) -> Result<()> {
        let (btn, _bits) = map_button(button);
        let params = DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MouseReleased)
            .x(x)
            .y(y)
            .button(btn)
            .buttons(0_i64)
            .click_count(1_i64)
            .build()
            .map_err(|e| GolemError::Browser(format!("mouse_release params: {e}")))?;
        self.execute_cmd(params).await
    }

    async fn mouse_wheel(&self, x: f64, y: f64, delta_x: f64, delta_y: f64) -> Result<()> {
        let params = DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MouseWheel)
            .x(x)
            .y(y)
            .delta_x(delta_x)
            .delta_y(delta_y)
            .build()
            .map_err(|e| GolemError::Browser(format!("mouse_wheel params: {e}")))?;
        self.execute_cmd(params).await
    }

    async fn key_char(&self, c: char) -> Result<()> {
        // Treat newline / carriage return as Enter.
        if c == '\n' || c == '\r' {
            return self.key_press("Enter").await;
        }
        let params = DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::Char)
            .text(c.to_string())
            .build()
            .map_err(|e| GolemError::Input(format!("key_char params: {e}")))?;
        self.execute_cmd(params).await
    }

    async fn key_type(&self, c: char) -> Result<()> {
        // Newlines/returns are an Enter key, not a printable char.
        if c == '\n' || c == '\r' {
            return self.key_press("Enter").await;
        }
        let s = c.to_string();
        let down = DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::KeyDown)
            .key(s.clone())
            .text(s.clone())
            .build()
            .map_err(|e| GolemError::Input(format!("key_type down params: {e}")))?;
        self.execute_cmd(down).await?;
        let up = DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::KeyUp)
            .key(s)
            .build()
            .map_err(|e| GolemError::Input(format!("key_type up params: {e}")))?;
        self.execute_cmd(up).await
    }

    async fn key_type_held(&self, c: char, hold: Duration) -> Result<()> {
        if c == '\n' || c == '\r' {
            return self.key_press_held("Enter", hold).await;
        }
        let s = c.to_string();
        let down = DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::KeyDown)
            .key(s.clone())
            .text(s.clone())
            .build()
            .map_err(|e| GolemError::Input(format!("key_type_held down params: {e}")))?;
        self.execute_cmd(down).await?;
        tokio::time::sleep(hold).await; // realistic key dwell (down -> up)
        let up = DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::KeyUp)
            .key(s)
            .build()
            .map_err(|e| GolemError::Input(format!("key_type_held up params: {e}")))?;
        self.execute_cmd(up).await
    }

    async fn key_press(&self, key: &str) -> Result<()> {
        let (name, code, vk) =
            key_table(key).ok_or_else(|| GolemError::Input(format!("unknown key: {key}")))?;
        let down = DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::KeyDown)
            .key(name)
            .code(code)
            .windows_virtual_key_code(vk)
            .build()
            .map_err(|e| GolemError::Input(format!("key down params: {e}")))?;
        self.execute_cmd(down).await?;
        let up = DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::KeyUp)
            .key(name)
            .code(code)
            .windows_virtual_key_code(vk)
            .build()
            .map_err(|e| GolemError::Input(format!("key up params: {e}")))?;
        self.execute_cmd(up).await
    }

    async fn key_press_held(&self, key: &str, hold: Duration) -> Result<()> {
        let (name, code, vk) =
            key_table(key).ok_or_else(|| GolemError::Input(format!("unknown key: {key}")))?;
        let down = DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::KeyDown)
            .key(name)
            .code(code)
            .windows_virtual_key_code(vk)
            .build()
            .map_err(|e| GolemError::Input(format!("key_press_held down params: {e}")))?;
        self.execute_cmd(down).await?;
        tokio::time::sleep(hold).await;
        let up = DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::KeyUp)
            .key(name)
            .code(code)
            .windows_virtual_key_code(vk)
            .build()
            .map_err(|e| GolemError::Input(format!("key_press_held up params: {e}")))?;
        self.execute_cmd(up).await
    }

    async fn switch_to_target(
        &self,
        url_substring: &str,
        exclude: &str,
        timeout: Duration,
    ) -> Result<bool> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // Clone the browser handle out of the lock so `pages()` (async) doesn't
            // hold a sync guard across the await.
            let browser = self.browser.read().clone();
            if let Some(b) = browser
                && let Ok(pages) = b.pages().await
            {
                // Pick the LAST (newest) page that matches and isn't excluded — so a
                // stale/expired session tab from an earlier run never wins.
                let mut chosen: Option<Page> = None;
                for p in pages {
                    if let Some(u) = page_url_opt(&p, self.config.call_timeout).await
                        && u.contains(url_substring)
                        && (exclude.is_empty() || !u.contains(exclude))
                    {
                        chosen = Some(p);
                    }
                }
                if let Some(p) = chosen {
                    // Make it the controlled page (keep it active even if occluded,
                    // like the initial attach does).
                    let _ = p.execute(SetFocusEmulationEnabledParams::new(true)).await;
                    *self.page.write() = Some(Arc::new(p));
                    return Ok(true);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    }

    async fn close_other_targets(&self, url_substring: &str) -> Result<usize> {
        // Keep whichever page we're currently driving; close every other tab whose
        // URL matches (e.g. a duplicate Vagon desktop). Never errors out — a tab we
        // fail to close just isn't counted.
        let keep = self.page.read().clone().map(|p| p.target_id().clone());
        let browser = self.browser.read().clone();
        let Some(b) = browser else { return Ok(0) };
        let Ok(pages) = b.pages().await else { return Ok(0) };
        let mut closed = 0usize;
        for p in pages {
            if Some(p.target_id()) == keep.as_ref() {
                continue;
            }
            let url = page_url_opt(&p, self.config.call_timeout).await;
            if url.is_some_and(|u| u.contains(url_substring)) && p.close().await.is_ok() {
                closed += 1;
            }
        }
        Ok(closed)
    }

    async fn bring_to_front(&self) -> Result<()> {
        let timeout = self.config.call_timeout;
        self.with_page("bring_to_front", async move |page: Arc<Page>| {
            match tokio::time::timeout(timeout, page.bring_to_front()).await {
                Err(_) => Err(GolemError::Timeout("bring_to_front".into())),
                Ok(Ok(_)) => Ok(()),
                Ok(Err(e)) => Err(GolemError::Browser(format!("bring_to_front: {e}"))),
            }
        })
        .await
    }

    async fn screenshot(&self) -> Result<Vec<u8>> {
        let timeout = self.config.call_timeout;
        self.with_page("screenshot", async move |page: Arc<Page>| {
            let params = ScreenshotParams::builder()
                .format(CaptureScreenshotFormat::Png)
                .build();
            match tokio::time::timeout(timeout, page.screenshot(params)).await {
                Err(_) => Err(GolemError::Timeout("screenshot".into())),
                Ok(Ok(bytes)) => Ok(bytes),
                Ok(Err(e)) => Err(GolemError::Browser(format!("screenshot: {e}"))),
            }
        })
        .await
    }

    async fn key_type_physical(&self, c: char) -> Result<()> {
        if c == '\n' || c == '\r' {
            return self.key_press("Enter").await;
        }
        let Some((code, vk, shift)) = char_key(c) else {
            // Unmapped char — fall back to the text-based path (works for browser
            // editors even if a scancode-forwarding stream drops it).
            return self.key_type(c).await;
        };
        let text = c.to_string();
        if shift {
            let sd = DispatchKeyEventParams::builder()
                .r#type(DispatchKeyEventType::KeyDown)
                .key("Shift")
                .code("ShiftLeft")
                .windows_virtual_key_code(16)
                .build()
                .map_err(|e| GolemError::Input(format!("shift down: {e}")))?;
            self.execute_cmd(sd).await?;
        }
        let down = DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::KeyDown)
            .key(text.clone())
            .code(code.clone())
            .text(text.clone())
            .windows_virtual_key_code(vk)
            .build()
            .map_err(|e| GolemError::Input(format!("char down: {e}")))?;
        self.execute_cmd(down).await?;
        let up = DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::KeyUp)
            .key(text)
            .code(code)
            .windows_virtual_key_code(vk)
            .build()
            .map_err(|e| GolemError::Input(format!("char up: {e}")))?;
        self.execute_cmd(up).await?;
        if shift {
            let su = DispatchKeyEventParams::builder()
                .r#type(DispatchKeyEventType::KeyUp)
                .key("Shift")
                .code("ShiftLeft")
                .windows_virtual_key_code(16)
                .build()
                .map_err(|e| GolemError::Input(format!("shift up: {e}")))?;
            self.execute_cmd(su).await?;
        }
        Ok(())
    }

    async fn viewport_size(&self) -> Result<(f64, f64)> {
        let v = self.eval_value("[window.innerWidth, window.innerHeight]").await?;
        let arr = v
            .as_array()
            .ok_or_else(|| GolemError::Browser("viewport size: not an array".into()))?;
        let w = arr.first().and_then(Value::as_f64).unwrap_or(0.0);
        let h = arr.get(1).and_then(Value::as_f64).unwrap_or(0.0);
        Ok((w, h))
    }

    async fn set_download_dir(&self, dir: &Path) -> Result<()> {
        let dir_str = dir.to_string_lossy().into_owned();
        let params = SetDownloadBehaviorParams::builder()
            .behavior(SetDownloadBehaviorBehavior::Allow)
            .download_path(dir_str)
            .build()
            .map_err(|e| GolemError::Browser(format!("download behavior params: {e}")))?;
        self.execute_cmd(params).await
    }

    async fn cookies_header(&self) -> Result<String> {
        let timeout = self.config.call_timeout;
        self.with_page("cookies", async move |page: Arc<Page>| {
            let cookies = match tokio::time::timeout(timeout, page.get_cookies()).await {
                Err(_) => return Err(GolemError::Timeout("get_cookies".into())),
                Ok(Ok(c)) => c,
                Ok(Err(e)) => return Err(GolemError::Browser(format!("get_cookies: {e}"))),
            };
            Ok(cookies
                .iter()
                .map(|c| format!("{}={}", c.name, c.value))
                .collect::<Vec<_>>()
                .join("; "))
        })
        .await
    }

    async fn user_agent(&self) -> Result<String> {
        let v = self.eval_value("navigator.userAgent").await?;
        Ok(v.as_str().unwrap_or_default().to_string())
    }
}

impl CdpBrowser {
    /// Single `page.url()` call wrapped in the call timeout.
    async fn url_once(&self) -> Result<Option<String>> {
        let page = self.page()?;
        match tokio::time::timeout(self.config.call_timeout, page.url()).await {
            Err(_) => Err(GolemError::Timeout("current_url".into())),
            Ok(Ok(opt)) => Ok(opt),
            Ok(Err(e)) => Err(GolemError::Browser(format!("url: {e}"))),
        }
    }
}

/// Map our backend button to the CDP button + the `buttons` bitmask bit
/// (Left=1, Right=2, Middle=4).
fn map_button(b: MouseButton) -> (CdpMouseButton, i64) {
    match b {
        MouseButton::Left => (CdpMouseButton::Left, 1),
        MouseButton::Right => (CdpMouseButton::Right, 2),
        MouseButton::Middle => (CdpMouseButton::Middle, 4),
    }
}

/// Named-key table → (`key`, `code`, Windows virtual key code).
fn key_table(key: &str) -> Option<(&'static str, &'static str, i64)> {
    let entry = match key {
        "Enter" => ("Enter", "Enter", 13),
        "Tab" => ("Tab", "Tab", 9),
        "Backspace" => ("Backspace", "Backspace", 8),
        "Escape" => ("Escape", "Escape", 27),
        "Delete" => ("Delete", "Delete", 46),
        "ArrowUp" => ("ArrowUp", "ArrowUp", 38),
        "ArrowDown" => ("ArrowDown", "ArrowDown", 40),
        "ArrowLeft" => ("ArrowLeft", "ArrowLeft", 37),
        "ArrowRight" => ("ArrowRight", "ArrowRight", 39),
        "Home" => ("Home", "Home", 36),
        "End" => ("End", "End", 35),
        "PageUp" => ("PageUp", "PageUp", 33),
        "PageDown" => ("PageDown", "PageDown", 34),
        // The Windows/Meta key + F11 — needed to drive the Vagon VM stream
        // (Win+R to open Run, F11 to fullscreen the terminal).
        "Meta" => ("Meta", "MetaLeft", 91),
        "F11" => ("F11", "F11", 122),
        _ => return None,
    };
    Some(entry)
}

/// Map a printable char to its US-keyboard physical (`code`, Windows virtual key
/// code, needs-shift), so a key event carries the scancode a remote desktop
/// (Vagon) forwards by. `None` for chars we don't map (caller falls back).
fn char_key(c: char) -> Option<(String, i64, bool)> {
    let r = match c {
        'a'..='z' => (format!("Key{}", c.to_ascii_uppercase()), c.to_ascii_uppercase() as i64, false),
        'A'..='Z' => (format!("Key{c}"), c as i64, true),
        '0'..='9' => (format!("Digit{c}"), c as i64, false),
        ' ' => ("Space".to_string(), 32, false),
        '.' => ("Period".to_string(), 190, false),
        ',' => ("Comma".to_string(), 188, false),
        '-' => ("Minus".to_string(), 189, false),
        '_' => ("Minus".to_string(), 189, true),
        '/' => ("Slash".to_string(), 191, false),
        '\\' => ("Backslash".to_string(), 220, false),
        ';' => ("Semicolon".to_string(), 186, false),
        ':' => ("Semicolon".to_string(), 186, true),
        '=' => ("Equal".to_string(), 187, false),
        '+' => ("Equal".to_string(), 187, true),
        // Shifted digits (netlist expressions / vim) — code stays the physical
        // Digit key; vk is the digit-key code; shift produces the symbol.
        '!' => ("Digit1".to_string(), 49, true),
        '@' => ("Digit2".to_string(), 50, true),
        '#' => ("Digit3".to_string(), 51, true),
        '$' => ("Digit4".to_string(), 52, true),
        '%' => ("Digit5".to_string(), 53, true),
        '^' => ("Digit6".to_string(), 54, true),
        '&' => ("Digit7".to_string(), 55, true),
        '*' => ("Digit8".to_string(), 56, true),
        '(' => ("Digit9".to_string(), 57, true),
        ')' => ("Digit0".to_string(), 48, true),
        '[' => ("BracketLeft".to_string(), 219, false),
        '{' => ("BracketLeft".to_string(), 219, true),
        ']' => ("BracketRight".to_string(), 221, false),
        '}' => ("BracketRight".to_string(), 221, true),
        '\'' => ("Quote".to_string(), 222, false),
        '"' => ("Quote".to_string(), 222, true),
        '`' => ("Backquote".to_string(), 192, false),
        '~' => ("Backquote".to_string(), 192, true),
        '<' => ("Comma".to_string(), 188, true),
        '>' => ("Period".to_string(), 190, true),
        '?' => ("Slash".to_string(), 191, true),
        '|' => ("Backslash".to_string(), 220, true),
        _ => return None,
    };
    Some(r)
}

/// Best-effort current URL of a page, swallowing errors to `None`.
async fn page_url_opt(page: &Page, timeout: Duration) -> Option<String> {
    match tokio::time::timeout(timeout, page.url()).await {
        Ok(Ok(opt)) => opt,
        _ => None,
    }
}

/// Resolve the `webSocketDebuggerUrl` from the DevTools `/json/version`
/// endpoint.
async fn resolve_ws_url(config: &ConnectionConfig) -> Result<String> {
    let url = format!("{}/json/version", config.devtools_http());
    let client = reqwest::Client::builder()
        .timeout(config.call_timeout)
        .build()
        .map_err(|e| GolemError::Connection(format!("http client: {e}")))?;
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| GolemError::Connection(format!("GET {url}: {e}")))?;
    let text = resp
        .text()
        .await
        .map_err(|e| GolemError::Connection(format!("read {url}: {e}")))?;
    let body: Value = serde_json::from_str(&text)
        .map_err(|e| GolemError::Connection(format!("decode {url}: {e}")))?;
    body.get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| GolemError::Connection("no webSocketDebuggerUrl in /json/version".into()))
}

/// Pick the target page to control: prefer a tab on the actual multimango.com
/// task site, else the first tab whose URL starts with `http`, else the first
/// page, else a fresh `about:blank` tab.
///
/// Right after `Browser::connect`, Chrome's target-discovery events (which
/// tell chromiumoxide what tabs already exist) may not have arrived yet, so
/// the very first `pages()` call can come back empty even when a real tab is
/// already open -- and we'd wrongly fall through to opening a blank one.
/// Retry briefly (up to ~1.5s) before giving up, rather than deciding "no
/// http tab" on a single, possibly-too-early check.
///
/// The multimango.com preference matters because a leftover tab sitting on a
/// raw asset URL (e.g. a `.../all_files.zip` link opened in a new tab while
/// testing downloads) also starts with `http` -- without this, whichever tab
/// `pages()` happens to list first can win, silently attaching Golem to a zip
/// URL instead of the real task page (no `<h1>` anywhere to find, every
/// selector-based lookup fails).
async fn pick_target(browser: &Browser) -> Result<Arc<Page>> {
    for attempt in 0..10 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        let pages = browser
            .pages()
            .await
            .map_err(|e| GolemError::Connection(format!("list pages: {e}")))?;
        let mut fallback: Option<Page> = None;
        for p in pages {
            if let Ok(Some(u)) = p.url().await
                && u.starts_with("http")
            {
                if u.contains("multimango.com") {
                    return Ok(Arc::new(p));
                }
                if fallback.is_none() {
                    fallback = Some(p);
                }
            }
        }
        if let Some(p) = fallback {
            return Ok(Arc::new(p));
        }
    }

    // No http(s) tab turned up after retrying -- fall back to whatever page
    // exists, else open a fresh blank tab as a last resort.
    let pages = browser
        .pages()
        .await
        .map_err(|e| GolemError::Connection(format!("list pages: {e}")))?;
    if let Some(p) = pages.into_iter().next() {
        return Ok(Arc::new(p));
    }
    let page = browser
        .new_page("about:blank")
        .await
        .map_err(|e| GolemError::Connection(format!("new page: {e}")))?;
    Ok(Arc::new(page))
}

/// Resolve, connect, spawn the handler driver, and pick a target. The returned
/// receiver fires once the underlying connection is dropped.
async fn establish(
    config: &ConnectionConfig,
) -> Result<(Browser, Arc<Page>, oneshot::Receiver<()>)> {
    let ws = resolve_ws_url(config).await?;
    let (browser, mut handler) = Browser::connect(ws)
        .await
        .map_err(|e| GolemError::Connection(format!("connect: {e}")))?;

    // Drive the handler stream to completion; signal on disconnect.
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        while handler.next().await.is_some() {}
        let _ = tx.send(());
    });

    let page = pick_target(&browser).await?;

    // Defeat background throttling: when Chrome's window is on another Wayland
    // workspace / occluded, the renderer is throttled and CDP-driven actions can
    // stall. Emulating focus keeps the page active regardless of window state.
    // Best-effort — ignore if the target doesn't support it.
    let _ = page.execute(SetFocusEmulationEnabledParams::new(true)).await;

    Ok((browser, page, rx))
}

/// [`establish`] guarded by the call timeout so a stuck handshake can't hang.
async fn establish_with_timeout(
    config: &ConnectionConfig,
) -> Result<(Browser, Arc<Page>, oneshot::Receiver<()>)> {
    match tokio::time::timeout(config.call_timeout, establish(config)).await {
        Err(_) => Err(GolemError::Connection("attach timed out".into())),
        Ok(r) => r,
    }
}

/// Whether the DevTools endpoint currently answers.
pub(crate) async fn endpoint_reachable(config: &ConnectionConfig) -> bool {
    resolve_ws_url(config).await.is_ok()
}

/// Browser binaries to try, Chrome first then Chromium.
const BROWSER_CANDIDATES: &[&str] = &[
    "google-chrome-stable",
    "google-chrome",
    "chrome",
    "chromium",
    "chromium-browser",
];

/// Launch Chrome/Chromium with the remote-debugging port enabled, in a DEDICATED
/// profile (modern Chrome refuses the debug socket on the default profile, so a
/// separate `--user-data-dir` is required). Uses `chrome_path` if set, else tries
/// Chrome then Chromium. The child is fully detached. Returns the binary used.
pub(crate) fn launch_debug_browser(
    port: u16,
    chrome_path: Option<&str>,
    user_data_dir: &str,
) -> Result<String> {
    let explicit = chrome_path.map(str::to_string);
    let candidates: Vec<&str> = match &explicit {
        Some(p) => vec![p.as_str()],
        None => BROWSER_CANDIDATES.to_vec(),
    };
    let mut last_err = String::from("no candidates");
    for bin in candidates {
        let mut cmd = std::process::Command::new(bin);
        cmd.arg(format!("--remote-debugging-port={port}"))
            .arg(format!("--user-data-dir={user_data_dir}"))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-background-timer-throttling")
            .arg("--disable-backgrounding-occluded-windows")
            .arg("--disable-renderer-backgrounding")
            .arg("about:blank")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        match cmd.spawn() {
            Ok(_child) => {
                tracing::info!("launched debug browser: {bin} (port {port})");
                return Ok(bin.to_string());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                last_err = format!("{bin}: not found on PATH");
            }
            Err(e) => last_err = format!("{bin}: {e}"),
        }
    }
    Err(GolemError::Connection(format!(
        "could not launch Chrome or Chromium ({last_err})"
    )))
}

/// Best-effort relaunch of Chrome with the debug flag. The child is detached
/// (not killed on drop) so it survives this process restarting.
fn relaunch_chrome(config: &ConnectionConfig) {
    let Some(path) = config.chrome_path.as_ref() else {
        return;
    };
    let mut cmd = tokio::process::Command::new(path);
    cmd.arg(format!("--remote-debugging-port={}", config.port));
    // Keep the renderer fully active even when the window is occluded / on
    // another workspace (otherwise CDP actions can stall — common on Wayland).
    cmd.arg("--disable-background-timer-throttling")
        .arg("--disable-backgrounding-occluded-windows")
        .arg("--disable-renderer-backgrounding");
    if let Some(dir) = config.user_data_dir.as_ref() {
        cmd.arg(format!("--user-data-dir={dir}"));
    }
    match cmd.spawn() {
        Ok(_child) => tracing::info!("relaunched chrome: {path}"),
        Err(e) => tracing::warn!("chrome relaunch failed: {e}"),
    }
}

/// Reconnect supervisor: waits for the connection to drop, then reattaches with
/// exponential backoff (optionally relaunching Chrome), swapping the controlled
/// page in place. Holds `browser` to keep the live connection alive.
async fn supervise(cdp: Arc<CdpBrowser>, mut disc_rx: oneshot::Receiver<()>) {
    let config = cdp.config.clone();
    let events = cdp.events.clone();

    loop {
        // Wait for the current connection to drop (sender dropped also counts).
        // Consuming `disc_rx` here is fine: it is reassigned before the next
        // iteration after a successful reconnect.
        let _ = disc_rx.await;
        tracing::warn!("cdp connection lost; starting reconnect");
        *cdp.page.write() = None;
        // Release the dead browser handle (drops the connection).
        *cdp.browser.write() = None;

        let mut attempt: u32 = 0;
        let mut backoff = config.reconnect_initial;

        let reconnected = loop {
            attempt = attempt.saturating_add(1);
            let _ = events.send(EngineEvent::Connection(ConnState::Reconnecting { attempt }));
            tokio::time::sleep(backoff).await;

            // Optionally relaunch Chrome if the endpoint is unreachable.
            if config.auto_relaunch
                && config.chrome_path.is_some()
                && !endpoint_reachable(&config).await
            {
                let _ = events.send(EngineEvent::Connection(ConnState::Relaunching));
                relaunch_chrome(&config);
                // Give the freshly spawned browser a moment to open its socket.
                tokio::time::sleep(backoff).await;
            }

            match establish_with_timeout(&config).await {
                Ok(triple) => break Some(triple),
                Err(e) => {
                    tracing::warn!("reconnect attempt {attempt} failed: {e}");
                    backoff = backoff
                        .checked_mul(2)
                        .unwrap_or(config.reconnect_max)
                        .min(config.reconnect_max);
                    if config.reconnect_max_attempts != 0
                        && attempt >= config.reconnect_max_attempts
                    {
                        break None;
                    }
                }
            }
        };

        match reconnected {
            Some((new_browser, page, rx)) => {
                let target_url = page_url_opt(&page, config.call_timeout).await;
                *cdp.page.write() = Some(page);
                *cdp.browser.write() = Some(Arc::new(new_browser));
                disc_rx = rx;
                let _ = events.send(EngineEvent::Connection(ConnState::Connected { target_url }));
                tracing::info!("cdp reconnected after {attempt} attempt(s)");
            }
            None => {
                tracing::error!("giving up reconnecting after {attempt} attempts");
                let _ = events.send(EngineEvent::Connection(ConnState::Disconnected));
                return;
            }
        }
    }
}
