//! The overlay window: created hidden once, shown on the monitor under the
//! pointer while the wheel or a HUD is up, never focused.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use knob_core::UiEvent;
use knob_platform::overlay;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

pub const LABEL: &str = "overlay";
pub const UI_EVENT: &str = "knob-ui";

/// Keep the window up this long after a close / HUD so the fade-outs finish.
const LINGER_AFTER_CLOSE: Duration = Duration::from_millis(250);
const LINGER_AFTER_HUD: Duration = Duration::from_millis(1600);

pub struct WheelWindow {
    app: AppHandle,
    wheel_open: AtomicBool,
    /// Bumped on every event; a pending hide only runs if nothing came after it.
    generation: AtomicU64,
}

pub fn create(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    let win = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("index.html".into()))
        .title("SlobKnob")
        .transparent(true)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .shadow(false)
        .focused(false)
        .visible(false)
        .visible_on_all_workspaces(true)
        .build()?;
    win.set_ignore_cursor_events(true)?;
    // SAFETY: setup runs on the main thread and the handle is live.
    unsafe { overlay::prepare(native_handle(&win)?) };
    Ok(win)
}

fn native_handle(win: &WebviewWindow) -> tauri::Result<*mut c_void> {
    #[cfg(target_os = "macos")]
    return win.ns_window();
    #[cfg(windows)]
    return win.hwnd().map(|h| h.0);
    #[cfg(not(any(target_os = "macos", windows)))]
    return Ok(std::ptr::null_mut());
}

/// Windows: once the overlay has been shown, WebView2 holds about 150 MB more
/// (mostly its GPU process). At the low memory target it lets go of that RAM
/// while the overlay is hidden; back to normal before it shows. Not set at
/// startup: before the first paint it saves little and makes the first open
/// about 150 ms slower.
#[cfg(windows)]
fn set_memory_low(win: &WebviewWindow, low: bool) {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2_19, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL,
    };
    use windows_core::Interface;
    let level = if low { COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW } else { COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL };
    let result = win.with_webview(move |webview| {
        // SAFETY: runs on the main thread with the webview's live controller.
        let set = unsafe {
            webview
                .controller()
                .CoreWebView2()
                .and_then(|core| core.cast::<ICoreWebView2_19>())
                .and_then(|core| core.SetMemoryUsageTargetLevel(level))
        };
        if let Err(e) = set {
            log::debug!("WebView2 memory target: {e}");
        }
    });
    if let Err(e) = result {
        log::debug!("WebView2 memory target: {e}");
    }
}

#[cfg(not(windows))]
fn set_memory_low(_win: &WebviewWindow, _low: bool) {}

impl WheelWindow {
    pub fn new(app: AppHandle) -> Self {
        Self { app, wheel_open: AtomicBool::new(false), generation: AtomicU64::new(0) }
    }

    /// Called from the runtime's worker thread.
    pub fn handle(&self, ui: UiEvent) {
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        match &ui {
            UiEvent::Open { .. } => {
                self.wheel_open.store(true, Ordering::Release);
                self.show();
            }
            UiEvent::Close => {
                self.wheel_open.store(false, Ordering::Release);
                self.hide_later(generation, LINGER_AFTER_CLOSE);
            }
            UiEvent::Hud { .. } => {
                self.show();
                self.hide_later(generation, LINGER_AFTER_HUD);
            }
            UiEvent::Select { .. } => {}
        }
        if let Err(e) = self.app.emit_to(LABEL, UI_EVENT, &ui) {
            log::error!("emit {UI_EVENT}: {e}");
        }
    }

    fn show(&self) {
        let app = self.app.clone();
        let _ = self.app.run_on_main_thread(move || {
            let Some(win) = app.get_webview_window(LABEL) else { return };
            cover_pointer_monitor(&app, &win);
            set_memory_low(&win, false);
            if let Ok(h) = native_handle(&win) {
                // SAFETY: on the main thread with a live window.
                unsafe { overlay::show(h) };
            }
        });
    }

    fn hide_later(&self, generation: u64, after: Duration) {
        let app = self.app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(after);
            let state = app.state::<crate::AppState>();
            let ww = &state.wheel_window;
            if ww.generation.load(Ordering::Acquire) != generation || ww.wheel_open.load(Ordering::Acquire) {
                return;
            }
            let app2 = app.clone();
            let _ = app.run_on_main_thread(move || {
                if let Some(win) = app2.get_webview_window(LABEL) {
                    if let Ok(h) = native_handle(&win) {
                        // SAFETY: on the main thread with a live window.
                        unsafe { overlay::hide(h) };
                    }
                    set_memory_low(&win, true);
                }
            });
        });
    }
}

fn cover_pointer_monitor(app: &AppHandle, win: &WebviewWindow) {
    let monitor = app
        .cursor_position()
        .ok()
        .and_then(|p| app.monitor_from_point(p.x, p.y).ok().flatten())
        .or_else(|| win.primary_monitor().ok().flatten());
    if let Some(m) = monitor {
        let _ = win.set_position(*m.position());
        let _ = win.set_size(*m.size());
    }
}
