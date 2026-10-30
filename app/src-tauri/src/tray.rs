//! Menu bar / system tray: pick a mode, settings, pause, open or reload the config, quit.

use std::sync::atomic::Ordering;
use std::sync::Mutex;

use knob_core::{mode_def, DEFAULT_WHEEL_KEY};
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{AppHandle, Manager, Wry};

use crate::AppState;

const TRAY_ID: &str = "slobknob";
const MODE_PREFIX: &str = "mode:";

/// Status line shown at the top of the menu (e.g. waiting for permission).
static STATUS: Mutex<Option<String>> = Mutex::new(None);

pub fn create(app: &AppHandle) -> tauri::Result<TrayIcon> {
    let icon = Image::from_bytes(include_bytes!("../icons/tray.png"))?;
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon)
        .icon_as_template(true)
        .tooltip("SlobKnob")
        .menu(&build_menu(app)?)
        .show_menu_on_left_click(true)
        .on_menu_event(on_menu_event)
        .build(app)
}

pub fn set_status(app: &AppHandle, text: &str) {
    *STATUS.lock().unwrap() = Some(text.to_string());
    refresh(app);
}

/// Rebuild the menu (current mode changed, config reloaded, ...).
pub fn refresh(app: &AppHandle) {
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let (Some(tray), Ok(menu)) = (app2.tray_by_id(TRAY_ID), build_menu(&app2)) {
            let _ = tray.set_menu(Some(menu));
        }
    });
}

fn build_menu(app: &AppHandle) -> tauri::Result<Menu<Wry>> {
    let state = app.state::<AppState>();
    let cfg = state.config.lock().unwrap().clone();
    let current = state.modes.lock().unwrap().get(DEFAULT_WHEEL_KEY).cloned();
    let running = state.runtime.lock().unwrap().is_some();
    let paused = state.paused.load(Ordering::Acquire);

    let menu = Menu::new(app)?;
    let status = if running {
        "SlobKnob".to_string()
    } else if paused {
        "SlobKnob: paused".to_string()
    } else {
        STATUS.lock().unwrap().clone().unwrap_or_else(|| "SlobKnob: starting…".into())
    };
    menu.append(&MenuItem::with_id(app, "status", status, false, None::<&str>)?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;

    let current = current.as_deref().unwrap_or(&cfg.wheel[0]);
    for id in &cfg.wheel {
        let Some(m) = mode_def(id) else { continue };
        let item = CheckMenuItem::with_id(app, format!("{MODE_PREFIX}{id}"), m.name, running, m.id == current, None::<&str>)?;
        menu.append(&item)?;
    }

    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(app, "settings", "Settings…", true, Some("CmdOrCtrl+,"))?)?;
    menu.append(&CheckMenuItem::with_id(app, "pause", "Pause", true, paused, None::<&str>)?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(app, "open-config", "Open Config File", true, None::<&str>)?)?;
    menu.append(&MenuItem::with_id(app, "reload-config", "Reload Config", true, None::<&str>)?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(app, "quit", "Quit SlobKnob", true, None::<&str>)?)?;
    Ok(menu)
}

fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    let id = event.id().as_ref();
    if let Some(mode) = id.strip_prefix(MODE_PREFIX) {
        if let Some(rt) = app.state::<AppState>().runtime.lock().unwrap().as_ref() {
            rt.set_mode_for_app(None, mode);
        }
        return;
    }
    match id {
        "settings" => crate::settings_window::open(app, None),
        "pause" => {
            let paused = app.state::<AppState>().paused.load(Ordering::Acquire);
            crate::set_paused(app, !paused);
        }
        "open-config" => {
            let path = app.state::<AppState>().paths.config.clone();
            open_target(&path.display().to_string());
        }
        "reload-config" => crate::reload_config(app),
        "quit" => app.exit(0),
        _ => {}
    }
}

/// Opens a file, folder or URL with its default app.
pub fn open_target(target: &str) {
    // Windows: ShellExecute via the `open` crate; `cmd /C start` mangles paths
    // with spaces, `&` or `^` and flashes a console.
    #[cfg(windows)]
    let result = open::that_detached(target);
    #[cfg(not(windows))]
    let result = {
        #[cfg(target_os = "macos")]
        let mut cmd = std::process::Command::new("open");
        #[cfg(not(target_os = "macos"))]
        let mut cmd = std::process::Command::new("xdg-open");
        cmd.arg(target).spawn().map(|_| ())
    };
    if let Err(e) = result {
        log::error!("open {target}: {e}");
    }
}
