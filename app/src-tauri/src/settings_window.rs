//! The settings window: a normal, focusable window created on demand and
//! closed (not hidden) when the user is done.
//!
//! While it is focused the runtime is in preview mode (unless the user turned
//! `preview_in_settings` off): the knob only moves the preview wheel in the
//! window. A preview flag left on makes the knob dead, so it is turned off on
//! blur, close and destroy, re-applied whenever a runtime starts or the config
//! changes, and a watchdog turns it off if the window is gone or unfocused.
//!
//! Windows: "focused" means the foreground window. The WebView2 child takes
//! keyboard focus as soon as the page is clicked, which reports the window as
//! unfocused while it is still in front, and leaving the app from there fires
//! no focus event at all. So focus events only trigger a foreground check, and
//! the watchdog follows the foreground both ways, quickly.
//!
//! Windows also skips the low-level keyboard hook while a WebView2 has focus,
//! so in front of this window knob keys never reach the engine and go to the
//! system instead (turning the volume). While the window is in front, the knob
//! keys (alone and with Win) are registered as hotkeys: those still fire,
//! consume the key and feed the runtime, preview or not. When the hook does see
//! a key it decides first, so nothing counts twice.

use std::sync::atomic::Ordering;
use std::time::Duration;

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};

use crate::AppState;

pub const LABEL: &str = "settings";
pub const LEARN_EVENT: &str = "knob-learn";
pub const PREVIEW_EVENT: &str = "knob-preview";

#[cfg(windows)]
const WATCHDOG_EVERY: Duration = Duration::from_millis(250);
#[cfg(not(windows))]
const WATCHDOG_EVERY: Duration = Duration::from_secs(2);

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
    let focus_win = win.clone();
    win.on_window_event(move |ev| match ev {
        WindowEvent::Focused(focused) => set_focused(&handle, if cfg!(windows) { is_focused(&focus_win) } else { *focused }),
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
    app.state::<AppState>().settings_in_front.store(focused, Ordering::Release);
    apply_preview(app);
}

fn window_gone(app: &AppHandle, destroyed: bool) {
    log::debug!("settings window {}; preview off", if destroyed { "destroyed" } else { "closing" });
    let state = app.state::<AppState>();
    state.settings_in_front.store(false, Ordering::Release);
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
/// sets the engine last also read the flag last. Windows: also syncs the
/// knob hotkeys.
pub fn apply_preview(app: &AppHandle) {
    let state = app.state::<AppState>();
    let preview_enabled = state.config.lock().unwrap().preview_in_settings;
    let guard = state.runtime.lock().unwrap();
    // Knob hotkeys: None = off, Some(learning).
    let mut hotkeys = None;
    if let Some(rt) = guard.as_ref() {
        let front = state.settings_in_front.load(Ordering::Acquire);
        let on = front && preview_enabled;
        log::trace!("preview {}", if on { "on" } else { "off" });
        rt.set_preview(on);
        if front && cfg!(windows) {
            hotkeys = Some(rt.inspect(|e| e.is_learning()));
        }
    }
    drop(guard);
    #[cfg(windows)]
    knob_hotkeys::sync(app, hotkeys);
    #[cfg(not(windows))]
    let _ = hotkeys;
}

#[cfg(windows)]
mod knob_hotkeys {
    use std::sync::Mutex;

    use knob_core::MediaKey;
    use tauri::{AppHandle, Manager};
    use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

    use crate::AppState;

    /// The media keys a hotkey can stand in for (not brightness: no VK).
    const KEYS: [(MediaKey, Code); 6] = [
        (MediaKey::VolumeUp, Code::AudioVolumeUp),
        (MediaKey::VolumeDown, Code::AudioVolumeDown),
        (MediaKey::Mute, Code::AudioVolumeMute),
        (MediaKey::PlayPause, Code::MediaPlayPause),
        (MediaKey::Next, Code::MediaTrackNext),
        (MediaKey::Previous, Code::MediaTrackPrevious),
    ];

    /// The media keys that should be hotkeys now; applied on the main thread.
    static WANTED: Mutex<Vec<MediaKey>> = Mutex::new(Vec::new());

    /// `learning`: None = no hotkeys; otherwise the knob's keys, or every key
    /// while learning (which swallows them all anyway and must see new ones).
    /// Cheap when nothing changed: it runs on every watchdog tick.
    pub fn sync(app: &AppHandle, learning: Option<bool>) {
        let keys: Vec<MediaKey> = match learning {
            None => Vec::new(),
            Some(true) => KEYS.map(|(key, _)| key).to_vec(),
            Some(false) => {
                let k = app.state::<AppState>().config.lock().unwrap().knob_keys;
                let knob = [k.clockwise, k.counter_clockwise, k.press];
                KEYS.into_iter().map(|(key, _)| key).filter(|key| knob.contains(key)).collect()
            }
        };
        {
            let mut wanted = WANTED.lock().unwrap();
            if *wanted == keys {
                return;
            }
            *wanted = keys;
        }
        let app2 = app.clone();
        let _ = app.run_on_main_thread(move || apply(&app2));
    }

    fn apply(app: &AppHandle) {
        let wanted = WANTED.lock().unwrap().clone();
        let gs = app.global_shortcut();
        for (key, code) in KEYS {
            // with Win too: Win + turn opens the wheel
            for modifier in [false, true] {
                let shortcut = Shortcut::new(modifier.then_some(Modifiers::SUPER), code);
                match (wanted.contains(&key), gs.is_registered(shortcut)) {
                    (true, false) => {
                        let result = gs.on_shortcut(shortcut, move |app, _shortcut, event| {
                            if let Some(rt) = app.state::<AppState>().runtime.lock().unwrap().as_ref() {
                                rt.feed_key(key, event.state == ShortcutState::Pressed, modifier);
                            }
                        });
                        if let Err(e) = result {
                            log::warn!("register {key:?} hotkey: {e}");
                        }
                    }
                    (false, true) => {
                        let _ = gs.unregister(shortcut);
                    }
                    _ => {}
                }
            }
        }
    }
}

fn is_focused(win: &WebviewWindow) -> bool {
    #[cfg(windows)]
    return win.hwnd().is_ok_and(|h| knob_platform::is_foreground(h.0));
    #[cfg(not(windows))]
    return win.is_focused().unwrap_or(false);
}

/// Belt and braces: every couple of seconds (Windows: every 250 ms, and both
/// ways), make sure the preview is only on while the settings window really
/// exists and has focus, and re-assert the wanted state on the engine (a no-op
/// when it already matches).
pub fn start_watchdog(app: AppHandle) {
    std::thread::Builder::new()
        .name("preview-watchdog".into())
        .spawn(move || loop {
            std::thread::sleep(WATCHDOG_EVERY);
            let state = app.state::<AppState>();
            let front = app.get_webview_window(LABEL).is_some_and(|w| is_focused(&w));
            if cfg!(windows) {
                state.settings_in_front.store(front, Ordering::Release);
            } else if state.settings_in_front.load(Ordering::Acquire) && !front {
                log::warn!("preview was on without a focused settings window; turning it off");
                state.settings_in_front.store(false, Ordering::Release);
            }
            apply_preview(&app);
        })
        .expect("spawn preview watchdog");
}
