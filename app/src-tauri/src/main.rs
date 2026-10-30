#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod settings;
mod settings_window;
mod tray;
mod wheel_window;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::Duration;

use knob_core::{Config, KnobKey, UiEvent};
use knob_platform::{input_permission, Callbacks, PlatformError, Runtime};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, RunEvent};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

use settings::Paths;
use wheel_window::WheelWindow;

pub struct AppState {
    paths: Paths,
    config: Mutex<Config>,
    modes: Mutex<HashMap<String, String>>,
    runtime: Mutex<Option<Runtime>>,
    wheel_window: WheelWindow,
    /// Paused from the tray: no runtime, no hook.
    paused: AtomicBool,
    /// A start_when_permitted loop is running.
    starting: AtomicBool,
    /// The thread dropping a paused runtime; joined before the next start.
    stopping: Mutex<Option<JoinHandle<()>>>,
    /// Whether the runtime should be in preview mode (settings window focused).
    /// Re-applied to every new runtime.
    preview_wanted: AtomicBool,
}

#[derive(Clone, Serialize)]
pub struct Theme {
    accent: String,
    dim: bool,
    platform: &'static str,
}

fn theme(cfg: &Config) -> Theme {
    let accent = if cfg.accent == "system" {
        knob_platform::accent_color()
            .map(|[r, g, b]| format!("#{r:02x}{g:02x}{b:02x}"))
            .unwrap_or_else(|| "#3478f6".into())
    } else {
        cfg.accent.clone()
    };
    Theme { accent, dim: cfg.dim_screen, platform: std::env::consts::OS }
}

#[tauri::command]
fn ui_ready(state: tauri::State<'_, AppState>) -> Theme {
    theme(&state.config.lock().unwrap())
}

const ESCAPE: &str = "Escape";

/// Esc closes the wheel. Registered only while it is open, so Esc keeps
/// working normally everywhere else.
fn set_escape(app: &AppHandle, wheel_open: bool) {
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        let gs = app2.global_shortcut();
        let registered = gs.is_registered(ESCAPE);
        if wheel_open && !registered {
            let result = gs.on_shortcut(ESCAPE, |app, _shortcut, event| {
                if event.state == ShortcutState::Pressed {
                    if let Some(rt) = app.state::<AppState>().runtime.lock().unwrap().as_ref() {
                        rt.cancel_wheel();
                    }
                }
            });
            if let Err(e) = result {
                log::warn!("register Esc: {e}");
            }
        } else if !wheel_open && registered {
            let _ = gs.unregister(ESCAPE);
        }
    });
}

fn start_runtime(app: &AppHandle) -> Result<Runtime, PlatformError> {
    let state = app.state::<AppState>();
    let cfg = state.config.lock().unwrap().clone();
    let modes = state.modes.lock().unwrap().clone();
    let ui_app = app.clone();
    let mode_app = app.clone();
    let learn_app = app.clone();
    let preview_app = app.clone();
    Runtime::start(
        cfg,
        modes,
        Callbacks {
            on_ui: Box::new(move |ui| {
                match ui {
                    UiEvent::Open { .. } => set_escape(&ui_app, true),
                    UiEvent::Close => set_escape(&ui_app, false),
                    _ => {}
                }
                ui_app.state::<AppState>().wheel_window.handle(ui);
            }),
            on_mode: Box::new(move |wheel, mode| {
                let state = mode_app.state::<AppState>();
                let mut modes = state.modes.lock().unwrap();
                modes.insert(wheel.to_string(), mode.to_string());
                settings::save_modes(&state.paths.state, &modes);
                drop(modes);
                tray::refresh(&mode_app);
            }),
            on_learn: Box::new(move |l| {
                log::info!("learn {l:?}");
                let _ = learn_app.emit_to(settings_window::LABEL, settings_window::LEARN_EVENT, l);
            }),
            on_preview: Box::new(move |key, down| {
                let _ = preview_app.emit_to(
                    settings_window::LABEL,
                    settings_window::PREVIEW_EVENT,
                    PreviewEvent { key, down },
                );
            }),
        },
    )
}

#[derive(Clone, Serialize)]
struct PreviewEvent {
    key: KnobKey,
    down: bool,
}

/// Starts the knob hook, waiting for the Accessibility permission if needed.
/// Does nothing while paused or if a start is already under way.
fn start_when_permitted(app: AppHandle) {
    let state = app.state::<AppState>();
    if state.paused.load(Ordering::Acquire) || state.starting.swap(true, Ordering::AcqRel) {
        return;
    }
    std::thread::spawn(move || {
        let state = app.state::<AppState>();
        // a paused runtime may still be shutting down; never run two hooks
        if let Some(t) = state.stopping.lock().unwrap().take() {
            let _ = t.join();
        }
        let mut asked = false;
        let mut failed = false;
        loop {
            if state.paused.load(Ordering::Acquire) || state.runtime.lock().unwrap().is_some() {
                break;
            }
            if input_permission(!asked) {
                match start_runtime(&app) {
                    Ok(rt) => {
                        *state.runtime.lock().unwrap() = Some(rt);
                        // the new engine starts with preview off
                        settings_window::apply_preview(&app);
                        log::info!("knob hook running");
                        break;
                    }
                    Err(PlatformError::PermissionDenied) => {}
                    Err(e) => {
                        log::error!("cannot start: {e}");
                        tray::set_status(&app, &format!("Error: {e}"));
                        failed = true;
                        break;
                    }
                }
            }
            if !asked {
                tray::set_status(&app, "Waiting for Accessibility permission…");
            }
            asked = true;
            std::thread::sleep(Duration::from_secs(2));
        }
        state.starting.store(false, Ordering::Release);
        if state.paused.load(Ordering::Acquire) {
            // paused while starting: undo
            stop_runtime(&app);
        } else if !failed && state.runtime.lock().unwrap().is_none() {
            // resumed again after this loop saw the pause (that resume found
            // `starting` set and left it to us)
            start_when_permitted(app.clone());
        }
        tray::refresh(&app);
    });
}

/// Drops the runtime (removing the hook) on a background thread, since Drop
/// joins the runtime's threads.
fn stop_runtime(app: &AppHandle) {
    let state = app.state::<AppState>();
    let rt = state.runtime.lock().unwrap().take();
    if let Some(rt) = rt {
        let t = std::thread::spawn(move || drop(rt));
        if let Some(old) = state.stopping.lock().unwrap().replace(t) {
            let _ = old.join();
        }
    }
    // a wheel or HUD that was up stays on screen otherwise
    state.wheel_window.handle(UiEvent::Close);
    set_escape(app, false);
}

pub fn set_paused(app: &AppHandle, paused: bool) {
    let state = app.state::<AppState>();
    if state.paused.swap(paused, Ordering::AcqRel) == paused {
        return;
    }
    if paused {
        stop_runtime(app);
        log::info!("paused");
        tray::refresh(app);
    } else {
        log::info!("resuming");
        start_when_permitted(app.clone());
        tray::refresh(app);
    }
}

pub fn reload_config(app: &AppHandle) {
    let state = app.state::<AppState>();
    let cfg = settings::load_config(&state.paths.config);
    *state.config.lock().unwrap() = cfg.clone();
    // set_config keeps the preview flag, so nothing to re-apply here
    let _ = app.emit_to(wheel_window::LABEL, "knob-theme", theme(&cfg));
    if let Some(rt) = state.runtime.lock().unwrap().as_ref() {
        rt.set_config(cfg);
    }
    tray::refresh(app);
}

/// `--settings` opens the settings window on launch; `--settings=<tab>` on that tab.
fn settings_flag() -> Option<String> {
    std::env::args().skip(1).find_map(|a| {
        if a == "--settings" {
            Some(String::new())
        } else {
            a.strip_prefix("--settings=").map(str::to_string)
        }
    })
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_autostart::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            ui_ready,
            commands::get_settings,
            commands::save_settings,
            commands::start_learn,
            commands::cancel_learn,
            commands::permissions,
            commands::request_permission,
            commands::open_privacy,
            commands::open_config_file,
            commands::preview_theme,
        ])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let handle = app.handle().clone();
            let paths = Paths::new(&handle);
            // before load_config, which writes the defaults
            let first_run = !paths.config.exists();
            let config = settings::load_config(&paths.config);
            let modes = settings::load_modes(&paths.state);
            app.manage(AppState {
                paths,
                config: Mutex::new(config),
                modes: Mutex::new(modes),
                runtime: Mutex::new(None),
                wheel_window: WheelWindow::new(handle.clone()),
                paused: AtomicBool::new(false),
                starting: AtomicBool::new(false),
                stopping: Mutex::new(None),
                preview_wanted: AtomicBool::new(false),
            });
            wheel_window::create(&handle)?;
            tray::create(&handle)?;
            settings_window::start_watchdog(handle.clone());
            if let Some(tab) = settings_flag().or(first_run.then(String::new)) {
                settings_window::open(&handle, (!tab.is_empty()).then_some(tab));
            }
            start_when_permitted(handle);
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building SlobKnob");

    app.run(|_app, event| {
        // menu bar app: closing the settings window must not quit
        if let RunEvent::ExitRequested { api, code: None, .. } = event {
            api.prevent_exit();
        }
    });
}
