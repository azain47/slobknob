//! Turns the wheel's window into an overlay that never takes focus: keystroke
//! modes (tabs, zoom, app switching) must keep going to the app underneath.
//!
//! `handle` is the native window: NSWindow* on macOS, HWND on Windows. All
//! functions must run on the UI (main) thread.

use std::ffi::c_void;

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::c_void;

    use objc2_app_kit::{NSWindow, NSWindowCollectionBehavior};

    /// NSScreenSaverWindowLevel: above full-screen apps and the menu bar.
    const LEVEL: isize = 1000;

    unsafe fn window<'a>(handle: *mut c_void) -> &'a NSWindow {
        // SAFETY: caller passes a live NSWindow* on the main thread.
        unsafe { &*(handle as *const NSWindow) }
    }

    pub unsafe fn prepare(handle: *mut c_void) {
        let w = unsafe { window(handle) };
        w.setLevel(LEVEL);
        w.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        w.setIgnoresMouseEvents(true);
        w.setHasShadow(false);
    }

    pub unsafe fn show(handle: *mut c_void) {
        // unlike makeKeyAndOrderFront, this neither activates the app nor takes key focus
        unsafe { window(handle) }.orderFrontRegardless();
    }

    pub unsafe fn hide(handle: *mut c_void) {
        unsafe { window(handle) }.orderOut(None);
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;

    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, ShowWindow, GWL_EXSTYLE, HWND_TOPMOST, SWP_NOACTIVATE,
        SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_HIDE, SW_SHOWNOACTIVATE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    };

    // Click-through (WS_EX_LAYERED | WS_EX_TRANSPARENT) is set by Tauri's
    // set_ignore_cursor_events; we add no-activate and keep it out of Alt+Tab.
    pub unsafe fn prepare(handle: *mut c_void) {
        let hwnd = HWND(handle);
        unsafe {
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | (WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0) as isize);
        }
    }

    pub unsafe fn show(handle: *mut c_void) {
        let hwnd = HWND(handle);
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
            );
        }
    }

    pub unsafe fn hide(handle: *mut c_void) {
        unsafe {
            let _ = ShowWindow(HWND(handle), SW_HIDE);
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    use std::ffi::c_void;
    pub unsafe fn prepare(_handle: *mut c_void) {}
    pub unsafe fn show(_handle: *mut c_void) {}
    pub unsafe fn hide(_handle: *mut c_void) {}
}

/// Overlay styling: on top of everything (incl. full-screen apps), every
/// desktop, no focus, no shadow, mouse passes through.
///
/// # Safety
/// `handle` must be a live native window, and this must run on the main thread.
pub unsafe fn prepare(handle: *mut c_void) {
    unsafe { imp::prepare(handle) }
}

/// Show without activating the app or taking keyboard focus.
///
/// # Safety
/// Same as [`prepare`].
pub unsafe fn show(handle: *mut c_void) {
    unsafe { imp::show(handle) }
}

/// # Safety
/// Same as [`prepare`].
pub unsafe fn hide(handle: *mut c_void) {
    unsafe { imp::hide(handle) }
}
