//! The settings window: a normal, focusable window created on demand and
//! closed (not hidden) when the user is done.
//!
//! While it is focused the runtime is in preview mode: the knob only moves the
//! preview wheel in the window. A preview flag left on makes the knob dead, so
//! it is turned off on blur, close and destroy, re-applied whenever a runtime
//! starts, and a watchdog turns it off if the window is gone or unfocused.

use std::sync::atomic::Ordering;
use std::time::Duration;

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};

use crate::AppState;

pub const LABEL: &str = "settings";
pub const LEARN_EVENT: &str = "knob-learn";
pub const PREVIEW_EVENT: &str = "knob-preview";

/// Opens (or focuses) the settings window, on `tab` if given. Safe from any thread.
pub fn open(app: &AppHandle, tab: Option<String>) {
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Err(e) = open_on_main(&app2, tab) {
            log::error!("settings window: {e}");
        }
    });
}

fn open_on_main(app: &AppHandle, tab: Option<String>) -> tauri::Result<()> {
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);

    if let Some(win) = app.get_webview_window(LABEL) {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
        if let Some(tab) = tab {
            let _ = win.eval(format!("window.knobSettings?.showTab({})", js_string(&tab)));
        }
        return Ok(());
    }

    let init = format!("window.__KW_INITIAL_TAB = {};", js_string(tab.as_deref().unwrap_or("")));
    let builder = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("settings.html".into()))
        .title("SlobKnob Settings")
        .inner_size(900.0, 640.0)
        .min_inner_size(720.0, 520.0)
        .theme(Some(tauri::Theme::Dark))
        .initialization_script(&init)
        .focused(true)
        .visible(true);

    // macOS: frosted sidebar under the traffic lights, like the overlay's glass.
    #[cfg(target_os = "macos")]
    let builder = builder
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
        .transparent(true)
        .effects(
            tauri::window::EffectsBuilder::new()
                .effect(tauri::window::Effect::UnderWindowBackground)
                .state(tauri::window::EffectState::FollowsWindowActiveState)
                .build(),
        );

    let win = builder.build()?;
    let handle = app.clone();
    win.on_window_event(move |ev| match ev {
        WindowEvent::Focused(focused) => set_focused(&handle, *focused),
        WindowEvent::CloseRequested { .. } => window_gone(&handle, false),
        WindowEvent::Destroyed => window_gone(&handle, true),
        _ => {}
    });
    let _ = win.set_focus();
    set_focused(app, true);
    debug_eval(&win);
    Ok(())
}

/// Debug builds: `SLOBKNOB_EVAL=<js>` runs a script in the window once it has
/// loaded, for testing the UI without clicking.
fn debug_eval(win: &WebviewWindow) {
    #[cfg(debug_assertions)]
    if let Ok(js) = std::env::var("SLOBKNOB_EVAL") {
        let win = win.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1500));
            let _ = win.eval(js);
        });
    }
    #[cfg(not(debug_assertions))]
    let _ = win;
}

fn js_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

fn set_focused(app: &AppHandle, focused: bool) {
    app.state::<AppState>().preview_wanted.store(focused, Ordering::Release);
    apply_preview(app);
}

fn window_gone(app: &AppHandle, destroyed: bool) {
    log::debug!("settings window {}; preview off", if destroyed { "destroyed" } else { "closing" });
    let state = app.state::<AppState>();
    state.preview_wanted.store(false, Ordering::Release);
    if let Some(rt) = state.runtime.lock().unwrap().as_ref() {
        rt.set_preview(false);
        // an abandoned "learn my knob" swallows every media key until it times out
        rt.cancel_learn();
    }
    #[cfg(target_os = "macos")]
    if destroyed {
        let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = destroyed;
}

/// Pushes the wanted preview state to the runtime, if one is running. The
/// flag is read under the runtime lock, so of two racing calls the one that
/// sets the engine last also read the flag last.
pub fn apply_preview(app: &AppHandle) {
    let state = app.state::<AppState>();
    let guard = state.runtime.lock().unwrap();
    if let Some(rt) = guard.as_ref() {
        let on = state.preview_wanted.load(Ordering::Acquire);
        log::trace!("preview {}", if on { "on" } else { "off" });
        rt.set_preview(on);
    }
}

fn is_focused(win: &WebviewWindow) -> bool {
    win.is_focused().unwrap_or(false)
}

/// Belt and braces: every couple of seconds, make sure the preview is only on
/// while the settings window really exists and has focus, and re-assert the
/// wanted state on the engine (a no-op when it already matches).
pub fn start_watchdog(app: AppHandle) {
    std::thread::Builder::new()
        .name("preview-watchdog".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_secs(2));
            let state = app.state::<AppState>();
            if state.preview_wanted.load(Ordering::Acquire)
                && !app.get_webview_window(LABEL).is_some_and(|w| is_focused(&w))
            {
                log::warn!("preview was on without a focused settings window; turning it off");
                state.preview_wanted.store(false, Ordering::Release);
            }
            apply_preview(&app);
        })
        .expect("spawn preview watchdog");
}
