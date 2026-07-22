//! Backend abstractions. `WorkflowCtx` is written entirely against these
//! traits, so the CDP backend (`crate::cdp`) and the native-input backend
//! (`crate::input`) are interchangeable and independently testable. Keeping the
//! humanization logic in `WorkflowCtx` (not the backends) means backends only
//! expose *primitive* operations.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;

use crate::error::Result;
use crate::geometry::Rect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// The browser (CDP) side. All coordinates are CSS pixels in the layout
/// viewport — the same space `Input.dispatchMouseEvent` and
/// `getBoundingClientRect()` use, which also works for clicking into an iframe
/// canvas (e.g. a remote-desktop stream) where no DOM is queryable.
#[async_trait]
pub trait BrowserBackend: Send + Sync {
    /// Navigate the controlled target to `url` and wait for load.
    async fn navigate(&self, url: &str) -> Result<()>;

    /// The controlled target's current URL.
    async fn current_url(&self) -> Result<String>;

    /// Switch the controlled target to a page (tab/popup) whose URL contains
    /// `url_substring` but NOT `exclude` (pass `""` to skip the exclusion — handy
    /// for stepping over a stale/expired session tab), picking the NEWEST such tab.
    /// Polls up to `timeout`. Returns `true` if it switched. Default: unsupported.
    async fn switch_to_target(
        &self,
        _url_substring: &str,
        _exclude: &str,
        _timeout: Duration,
    ) -> Result<bool> {
        Ok(false)
    }

    /// Close every page/tab whose URL contains `url_substring` EXCEPT the one
    /// currently being controlled. Returns how many were closed. Used to clear
    /// duplicate Vagon desktops (a blocked-then-retried connect, or a stale
    /// session) so two streams can't fight over the same workstation. Default:
    /// no-op (0 closed).
    async fn close_other_targets(&self, _url_substring: &str) -> Result<usize> {
        Ok(0)
    }

    /// Bring the controlled target to the FOREGROUND (activate its tab). Some
    /// remote-desktop streams (Vagon) only capture keyboard input while their tab
    /// is actually in front. Default: no-op.
    async fn bring_to_front(&self) -> Result<()> {
        Ok(())
    }

    /// Capture a PNG screenshot of the controlled page (composited surface, so a
    /// streamed `<video>` is included). Default: unsupported.
    async fn screenshot(&self) -> Result<Vec<u8>> {
        Err(crate::error::GolemError::Browser("screenshot unsupported".into()))
    }

    /// Resolve when `selector` appears (or error on timeout).
    async fn wait_for_selector(&self, selector: &str, timeout: Duration) -> Result<()>;

    /// Whether at least one node matches `selector` right now.
    async fn query_exists(&self, selector: &str) -> Result<bool>;

    /// `getAttribute` of the first match, or `None` if no match / no attribute.
    async fn get_attribute(&self, selector: &str, name: &str) -> Result<Option<String>>;

    /// `innerText`/`textContent` of the first match.
    async fn get_text(&self, selector: &str) -> Result<Option<String>>;

    /// Viewport-space bounding rect (CSS px) of the first match, scrolling it
    /// into view first. `None` if no match.
    async fn get_rect(&self, selector: &str) -> Result<Option<Rect>>;

    /// Evaluate JavaScript in the page and return the JSON result. The escape
    /// hatch for "anything CDP can do" at the DOM/JS level.
    async fn eval(&self, js: &str) -> Result<serde_json::Value>;

    /// Move focus to the first match (for typing).
    async fn focus(&self, selector: &str) -> Result<()>;

    // --- primitive input (CSS-pixel viewport coords) ---
    async fn mouse_move(&self, x: f64, y: f64) -> Result<()>;
    async fn mouse_press(&self, button: MouseButton, x: f64, y: f64) -> Result<()>;
    async fn mouse_release(&self, button: MouseButton, x: f64, y: f64) -> Result<()>;
    async fn mouse_wheel(&self, x: f64, y: f64, delta_x: f64, delta_y: f64) -> Result<()>;

    /// Type a single character as a "Char" event (inserts text into a focused
    /// input/contenteditable). Best for web forms.
    async fn key_char(&self, c: char) -> Result<()>;
    /// Type a single character as a real key DOWN+UP with `key`/`text` set, so
    /// apps that process `keydown` (terminals, browser-hosted editors like
    /// neovim) receive it. Best for the "complete task" typing.
    async fn key_type(&self, c: char) -> Result<()>;
    /// Press a named key, e.g. `"Enter"`, `"Tab"`, `"Backspace"`, `"Escape"`.
    async fn key_press(&self, key: &str) -> Result<()>;

    /// Type a character as a PHYSICAL key event (code + virtual-key code + shift
    /// for uppercase/symbols), so a remote-desktop stream (Vagon) that forwards
    /// keystrokes by scancode actually receives it. Default: falls back to
    /// [`key_type`](Self::key_type) (fine for browser editors).
    async fn key_type_physical(&self, c: char) -> Result<()> {
        self.key_type(c).await
    }

    /// Like [`key_type`](Self::key_type) but holds the key DOWN for `hold` before
    /// releasing, giving a realistic dwell time. Default ignores the hold.
    async fn key_type_held(&self, c: char, _hold: Duration) -> Result<()> {
        self.key_type(c).await
    }
    /// Like [`key_press`](Self::key_press) but holds the key for `hold`. Default
    /// ignores the hold.
    async fn key_press_held(&self, key: &str, _hold: Duration) -> Result<()> {
        self.key_press(key).await
    }

    /// Current viewport size in CSS px `(width, height)`.
    async fn viewport_size(&self) -> Result<(f64, f64)>;

    /// Direct Chrome to save downloads to `dir`.
    async fn set_download_dir(&self, dir: &Path) -> Result<()>;

    /// A `Cookie:` header value for the current target's origin, so files can be
    /// fetched with `reqwest` using the live session.
    async fn cookies_header(&self) -> Result<String>;

    /// The raw `User-Agent` of the controlled session (for matching downloads).
    async fn user_agent(&self) -> Result<String>;
}

/// The native OS input side (real cursor + keyboard). Used for OS-level dialogs
/// (file open/save) and as the `Native`/`Hybrid` input strategy. Coordinates
/// are absolute *screen* pixels.
#[async_trait]
pub trait InputBackend: Send + Sync {
    async fn mouse_move_abs(&self, x: i32, y: i32) -> Result<()>;
    async fn mouse_press(&self, button: MouseButton) -> Result<()>;
    async fn mouse_release(&self, button: MouseButton) -> Result<()>;
    async fn cursor_pos(&self) -> Result<(i32, i32)>;
    async fn scroll(&self, delta_x: i32, delta_y: i32) -> Result<()>;

    /// Type a single character via the OS.
    async fn key_char(&self, c: char) -> Result<()>;
    /// Press + release a named key.
    async fn key_press(&self, key: &str) -> Result<()>;
    async fn key_down(&self, key: &str) -> Result<()>;
    async fn key_up(&self, key: &str) -> Result<()>;

    /// Whether this backend can actually inject on the current platform/session
    /// (e.g. `false` on a locked-down Wayland session). Lets the engine warn
    /// instead of silently doing nothing.
    fn is_available(&self) -> bool {
        true
    }
}
