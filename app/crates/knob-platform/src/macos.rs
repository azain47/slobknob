//! macOS: a CGEventTap for NSSystemDefined (media keys) and flagsChanged
//! (to track Cmd) events only, so regular typing never passes through us.

use std::ffi::c_void;
use std::process::Command;
use std::ptr::{self, NonNull};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{mpsc, Condvar, Mutex, OnceLock, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use knob_core::{Axis, DeviceInfo, Key, KeyCombo, MediaKey, Mods, NamedKey};
use objc2::rc::autoreleasepool;
use objc2_app_kit::{
    NSColor, NSColorSpace, NSEvent, NSEventModifierFlags, NSEventType, NSRunningApplication, NSWorkspace,
};
use objc2_core_foundation::{kCFRunLoopCommonModes, CFMachPort, CFRetained, CFRunLoop, CGPoint};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType, CGKeyCode, CGMainDisplayID, CGMouseButton, CGScrollEventUnit,
    CGWarpMouseCursorPosition,
};
use objc2_foundation::{NSDictionary, NSNumber, NSPoint, NSString};

use crate::{call_handler, Action, HookEvent, InputEvent, KeyHandler, PlatformError};

/// Stamped into kCGEventSourceUserData of every event we post, so our own tap
/// lets them through and never mistakes them for the knob or a physical Cmd.
const MAGIC: i64 = 0x4b4e_4f42; // "KNOB"

const SYSTEM_DEFINED: CGEventType = CGEventType(14);
/// NSSystemDefined subtype for media / aux control buttons.
const AUX_CONTROL_BUTTONS: i16 = 8;

// NX_KEYTYPE_* codes carried in data1 of media key events.
const NX_SOUND_UP: isize = 0;
const NX_SOUND_DOWN: isize = 1;
const NX_BRIGHTNESS_UP: isize = 2;
const NX_BRIGHTNESS_DOWN: isize = 3;
const NX_MUTE: isize = 7;
const NX_PLAY: isize = 16;
const NX_NEXT: isize = 17;
const NX_PREVIOUS: isize = 18;

// Virtual key codes (ANSI layout positions).
mod vk {
    use objc2_core_graphics::CGKeyCode;
    pub const Z: CGKeyCode = 6;
    pub const EQUAL: CGKeyCode = 24;
    pub const MINUS: CGKeyCode = 27;
    pub const ZERO: CGKeyCode = 29;
    pub const RIGHT_BRACKET: CGKeyCode = 30;
    pub const LEFT_BRACKET: CGKeyCode = 33;
    pub const RETURN: CGKeyCode = 36;
    pub const TAB: CGKeyCode = 48;
    pub const SPACE: CGKeyCode = 49;
    pub const GRAVE: CGKeyCode = 50;
    pub const BACKSPACE: CGKeyCode = 51;
    pub const ESCAPE: CGKeyCode = 53;
    pub const COMMAND: CGKeyCode = 55;
    pub const HOME: CGKeyCode = 115;
    pub const PAGE_UP: CGKeyCode = 116;
    pub const FORWARD_DELETE: CGKeyCode = 117;
    pub const END: CGKeyCode = 119;
    pub const PAGE_DOWN: CGKeyCode = 121;
    pub const LEFT: CGKeyCode = 123;
    pub const RIGHT: CGKeyCode = 124;
    pub const DOWN: CGKeyCode = 125;
    pub const UP: CGKeyCode = 126;
    /// F1..=F20.
    pub const F: [CGKeyCode; 20] =
        [122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111, 105, 107, 113, 106, 64, 79, 80, 90];

    /// ANSI key for a letter, digit or punctuation character.
    pub fn for_char(c: char) -> Option<CGKeyCode> {
        const ROW: &[(char, CGKeyCode)] = &[
            ('a', 0), ('s', 1), ('d', 2), ('f', 3), ('h', 4), ('g', 5), ('z', 6), ('x', 7), ('c', 8), ('v', 9),
            ('b', 11), ('q', 12), ('w', 13), ('e', 14), ('r', 15), ('y', 16), ('t', 17), ('1', 18), ('2', 19),
            ('3', 20), ('4', 21), ('6', 22), ('5', 23), ('=', 24), ('9', 25), ('7', 26), ('-', 27), ('8', 28),
            ('0', 29), (']', 30), ('o', 31), ('u', 32), ('[', 33), ('i', 34), ('p', 35), ('l', 37), ('j', 38),
            ('\'', 39), ('k', 40), (';', 41), ('\\', 42), (',', 43), ('/', 44), ('n', 45), ('m', 46), ('.', 47),
            ('`', 50),
        ];
        ROW.iter().find(|(ch, _)| *ch == c).map(|(_, code)| *code)
    }
}

const CMD: CGEventFlags = CGEventFlags::MaskCommand;
const SHIFT: CGEventFlags = CGEventFlags::MaskShift;
const CTRL: CGEventFlags = CGEventFlags::MaskControl;
const ALT: CGEventFlags = CGEventFlags::MaskAlternate;
const NONE: CGEventFlags = CGEventFlags(0);

// --- hook -------------------------------------------------------------------

static HANDLER: RwLock<Option<KeyHandler>> = RwLock::new(None);
/// The tap's mach port, so the callback can re-enable it after a timeout.
static TAP_PORT: AtomicPtr<CFMachPort> = AtomicPtr::new(ptr::null_mut());
/// Physical Cmd state (our own synthetic Cmd presses are ignored).
static CMD_DOWN: AtomicBool = AtomicBool::new(false);
/// Down state per NX key code we report, to mark auto-repeat.
static KEY_DOWN: [AtomicBool; 32] = [const { AtomicBool::new(false) }; 32];

struct RunLoopRef(CFRetained<CFRunLoop>);
// SAFETY: CFRunLoopStop may be called from any thread; we only use it for that.
unsafe impl Send for RunLoopRef {}

pub struct Hook {
    run_loop: RunLoopRef,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Hook {
    fn drop(&mut self) {
        self.run_loop.0.stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        *HANDLER.write().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

pub fn start_hook(handler: KeyHandler) -> Result<Hook, PlatformError> {
    *HANDLER.write().unwrap_or_else(|e| e.into_inner()) = Some(handler);
    let (tx, rx) = mpsc::channel::<Result<RunLoopRef, PlatformError>>();
    let thread = std::thread::Builder::new()
        .name("knob-hook".into())
        .spawn(move || run_tap(tx))
        .map_err(|e| PlatformError::Other(e.to_string()))?;
    match rx.recv() {
        Ok(Ok(run_loop)) => Ok(Hook { run_loop, thread: Some(thread) }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => Err(PlatformError::Other("hook thread exited".into())),
    }
}

fn run_tap(tx: mpsc::Sender<Result<RunLoopRef, PlatformError>>) {
    let mask = (1u64 << SYSTEM_DEFINED.0) | (1u64 << CGEventType::FlagsChanged.0);
    // SAFETY: the callback matches CGEventTapCallBack and ignores user_info.
    let port = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            mask,
            Some(tap_callback),
            ptr::null_mut(),
        )
    };
    // No port means no Accessibility permission (or another tap failure).
    let Some(port) = port else {
        let _ = tx.send(Err(PlatformError::PermissionDenied));
        return;
    };
    let Some(source) = CFMachPort::new_run_loop_source(None, Some(&port), 0) else {
        let _ = tx.send(Err(PlatformError::Other("no run loop source for event tap".into())));
        return;
    };
    let run_loop = CFRunLoop::current().expect("thread has a run loop");
    // SAFETY: kCFRunLoopCommonModes is a valid static CFString.
    run_loop.add_source(Some(&source), unsafe { kCFRunLoopCommonModes });
    TAP_PORT.store(CFRetained::as_ptr(&port).as_ptr(), Ordering::Release);
    CGEvent::tap_enable(&port, true);
    let _ = tx.send(Ok(RunLoopRef(run_loop)));

    CFRunLoop::run();

    TAP_PORT.store(ptr::null_mut(), Ordering::Release);
    port.invalidate();
}

unsafe extern "C-unwind" fn tap_callback(
    _proxy: CGEventTapProxy,
    kind: CGEventType,
    event: NonNull<CGEvent>,
    _user_info: *mut c_void,
) -> *mut CGEvent {
    let pass = event.as_ptr();
    // SAFETY: the system hands us a valid event for the duration of the call.
    let event_ref = unsafe { event.as_ref() };

    if kind == CGEventType::TapDisabledByTimeout || kind == CGEventType::TapDisabledByUserInput {
        let port = TAP_PORT.load(Ordering::Acquire);
        if let Some(port) = unsafe { port.as_ref() } {
            CGEvent::tap_enable(port, true);
        }
        return pass;
    }
    if CGEvent::integer_value_field(Some(event_ref), CGEventField::EventSourceUserData) == MAGIC {
        return pass;
    }

    if kind == CGEventType::FlagsChanged {
        let cmd = CGEvent::flags(Some(event_ref)).contains(CMD);
        if CMD_DOWN.swap(cmd, Ordering::AcqRel) && !cmd {
            dispatch(HookEvent::ModifierReleased);
        }
        return pass;
    }

    if kind != SYSTEM_DEFINED {
        return pass;
    }
    let Some((code, key, down, repeat)) = autoreleasepool(|_| media_key(event_ref)) else {
        return pass;
    };
    let was_down = KEY_DOWN[code as usize].swap(down, Ordering::AcqRel);
    let repeat = repeat || (down && was_down);
    let device = if LISTENING.load(Ordering::Acquire) && !repeat { device_for(key, down) } else { None };
    let ev = InputEvent { key, down, repeat, modifier: CMD_DOWN.load(Ordering::Acquire), device };
    match dispatch(HookEvent::Key(ev)) {
        knob_core::Decision::Swallow => ptr::null_mut(),
        knob_core::Decision::Pass => pass,
    }
}

fn dispatch(ev: HookEvent) -> knob_core::Decision {
    match HANDLER.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
        Some(handler) => call_handler(handler, ev),
        None => knob_core::Decision::Pass,
    }
}

/// Decodes a media key event into (NX code, key, down, repeat). Other system
/// events give None.
fn media_key(event: &CGEvent) -> Option<(isize, MediaKey, bool, bool)> {
    let ns = NSEvent::eventWithCGEvent(event)?;
    if ns.subtype().0 != AUX_CONTROL_BUTTONS {
        return None;
    }
    let data1 = ns.data1();
    let code = (data1 & 0xffff_0000) >> 16;
    let flags = data1 & 0xffff;
    let down = (flags & 0xff00) >> 8 == 0xa;
    let repeat = flags & 0x1 != 0;
    let key = match code {
        NX_SOUND_UP => MediaKey::VolumeUp,
        NX_SOUND_DOWN => MediaKey::VolumeDown,
        NX_MUTE => MediaKey::Mute,
        NX_PLAY => MediaKey::PlayPause,
        NX_NEXT => MediaKey::Next,
        NX_PREVIOUS => MediaKey::Previous,
        NX_BRIGHTNESS_UP => MediaKey::BrightnessUp,
        NX_BRIGHTNESS_DOWN => MediaKey::BrightnessDown,
        _ => return None,
    };
    Some((code, key, down, repeat))
}

// --- actions ----------------------------------------------------------------

#[derive(Default)]
struct Held {
    cmd_for_app_switch: bool,
    drag: Option<Drag>,
}

struct Drag {
    origin: CGPoint,
    pos: CGPoint,
}

static HELD: Mutex<Held> = Mutex::new(Held { cmd_for_app_switch: false, drag: None });

/// Takes CFRetained (from CoreGraphics) or Retained (from NSEvent::CGEvent).
fn post<E: std::ops::Deref<Target = CGEvent>>(event: Option<E>) {
    if let Some(e) = event {
        CGEvent::set_integer_value_field(Some(&e), CGEventField::EventSourceUserData, MAGIC);
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&e));
    }
}

fn key_event(code: CGKeyCode, down: bool, flags: CGEventFlags) {
    let e = CGEvent::new_keyboard_event(None, code, down);
    if let Some(e) = &e {
        CGEvent::set_flags(Some(e), flags);
    }
    post(e);
}

fn stroke(flags: CGEventFlags, code: CGKeyCode) {
    key_event(code, true, flags);
    key_event(code, false, flags);
}

fn system_key(code: isize) {
    for down in [true, false] {
        let state: isize = if down { 0xa } else { 0xb };
        let e = autoreleasepool(|_| {
            NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
                NSEventType::SystemDefined,
                NSPoint::new(0.0, 0.0),
                NSEventModifierFlags((state << 8) as usize),
                0.0,
                0,
                None,
                AUX_CONTROL_BUTTONS,
                (code << 16) | (state << 8),
                -1,
            )
            .and_then(|ns| ns.CGEvent())
        });
        post(e);
    }
}

fn pointer() -> CGPoint {
    CGEvent::new(None).map(|e| CGEvent::location(Some(&e))).unwrap_or(CGPoint::new(0.0, 0.0))
}

fn mouse(kind: CGEventType, at: CGPoint) {
    post(CGEvent::new_mouse_event(None, kind, at, CGMouseButton::Left));
}

pub fn perform(action: &Action) {
    let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
    let back = |dir: i8| dir < 0;
    match *action {
        Action::Media(MediaKey::BrightnessUp) if set_brightness_by(BRIGHTNESS_STEP) => {}
        Action::Media(MediaKey::BrightnessDown) if set_brightness_by(-BRIGHTNESS_STEP) => {}
        Action::Media(key) => system_key(match key {
            MediaKey::VolumeUp => NX_SOUND_UP,
            MediaKey::VolumeDown => NX_SOUND_DOWN,
            MediaKey::Mute => NX_MUTE,
            MediaKey::PlayPause => NX_PLAY,
            MediaKey::Next => NX_NEXT,
            MediaKey::Previous => NX_PREVIOUS,
            MediaKey::BrightnessUp => NX_BRIGHTNESS_UP,
            MediaKey::BrightnessDown => NX_BRIGHTNESS_DOWN,
        }),
        Action::SmoothScroll { axis, pixels } => smooth_scroll(axis, pixels),
        Action::Shortcut(ref combos) => {
            for combo in combos {
                shortcut(combo);
            }
        }
        Action::Open(ref target) => open(target),
        Action::TypeText(ref text) => type_text(text),
        Action::Scroll { axis, lines } => {
            // Quartz: positive wheel1 scrolls up, positive wheel2 scrolls left.
            let (v, h) = match axis {
                Axis::Vertical => (-lines, 0),
                Axis::Horizontal => (0, -lines),
            };
            post(CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Line, 2, v, h, 0));
        }
        Action::ZoomIn => stroke(CMD, vk::EQUAL),
        Action::ZoomOut => stroke(CMD, vk::MINUS),
        Action::ZoomReset => stroke(CMD, vk::ZERO),
        Action::AppSwitchStep(dir) => {
            if !held.cmd_for_app_switch {
                key_event(vk::COMMAND, true, CMD);
                held.cmd_for_app_switch = true;
            }
            stroke(if back(dir) { CMD | SHIFT } else { CMD }, vk::TAB);
        }
        Action::AppSwitchEnd => {
            if std::mem::take(&mut held.cmd_for_app_switch) {
                key_event(vk::COMMAND, false, NONE);
            }
        }
        Action::WindowCycle(dir) => stroke(if back(dir) { CMD | SHIFT } else { CMD }, vk::GRAVE),
        Action::TabCycle(dir) => stroke(CMD | SHIFT, if back(dir) { vk::LEFT_BRACKET } else { vk::RIGHT_BRACKET }),
        Action::DesktopStep(dir) => stroke(CTRL, if back(dir) { vk::LEFT } else { vk::RIGHT }),
        Action::DesktopOverview => stroke(CTRL, vk::UP),
        Action::Seek { seconds } => seek(seconds),
        Action::DragStep { axis, pixels } => {
            let drag = held.drag.get_or_insert_with(|| {
                let origin = pointer();
                mouse(CGEventType::LeftMouseDown, origin);
                Drag { origin, pos: origin }
            });
            match axis {
                Axis::Horizontal => drag.pos.x += pixels as f64,
                Axis::Vertical => drag.pos.y -= pixels as f64,
            }
            mouse(CGEventType::LeftMouseDragged, drag.pos);
        }
        Action::DragEnd => {
            if let Some(drag) = held.drag.take() {
                mouse(CGEventType::LeftMouseUp, drag.pos);
                CGWarpMouseCursorPosition(drag.origin);
            }
        }
        Action::FrameStep(dir) => stroke(NONE, if back(dir) { vk::LEFT } else { vk::RIGHT }),
        Action::EditPoint(dir) => stroke(NONE, if back(dir) { vk::UP } else { vk::DOWN }),
        Action::SpaceKey => stroke(NONE, vk::SPACE),
        Action::UndoRedo(dir) => stroke(if back(dir) { CMD } else { CMD | SHIFT }, vk::Z),
    }
}

fn shortcut(combo: &KeyCombo) {
    let code = match combo.key {
        Key::Char(c) => vk::for_char(c),
        Key::Named(n) => Some(match n {
            NamedKey::Enter => vk::RETURN,
            NamedKey::Tab => vk::TAB,
            NamedKey::Space => vk::SPACE,
            NamedKey::Escape => vk::ESCAPE,
            NamedKey::Backspace => vk::BACKSPACE,
            NamedKey::Delete => vk::FORWARD_DELETE,
            NamedKey::Up => vk::UP,
            NamedKey::Down => vk::DOWN,
            NamedKey::Left => vk::LEFT,
            NamedKey::Right => vk::RIGHT,
            NamedKey::Home => vk::HOME,
            NamedKey::End => vk::END,
            NamedKey::PageUp => vk::PAGE_UP,
            NamedKey::PageDown => vk::PAGE_DOWN,
            NamedKey::F(n) => vk::F[(n as usize).clamp(1, 20) - 1],
        }),
    };
    let Some(code) = code else {
        log::warn!("no key code for {combo:?}");
        return;
    };
    let m = combo.mods;
    let mut flags = NONE;
    for (bit, flag) in [(Mods::META, CMD), (Mods::PRIMARY, CMD), (Mods::CTRL, CTRL), (Mods::ALT, ALT), (Mods::SHIFT, SHIFT)] {
        if m.contains(bit) {
            flags |= flag;
        }
    }
    stroke(flags, code);
}

/// Unicode text in chunks; CGEventKeyboardSetUnicodeString takes at most 20
/// UTF-16 units per event. Layout independent.
fn type_text(text: &str) {
    let units: Vec<u16> = text.encode_utf16().collect();
    for chunk in units.chunks(20) {
        for down in [true, false] {
            let e = CGEvent::new_keyboard_event(None, 0, down);
            if let Some(e) = &e {
                CGEvent::set_flags(Some(e), NONE);
                // SAFETY: chunk is a live slice of chunk.len() UTF-16 units.
                unsafe { CGEvent::keyboard_set_unicode_string(Some(e), chunk.len() as _, chunk.as_ptr()) };
            }
            post(e);
        }
        // some apps drop characters that arrive in one burst
        std::thread::sleep(Duration::from_millis(3));
    }
}

/// URL or path: `open <target>`. Anything else is an app name: `open -a`,
/// falling back to plain `open`. Runs off the worker thread.
fn open(target: &str) {
    let target = match target.strip_prefix("~/") {
        Some(rest) => std::env::var("HOME").map(|h| format!("{h}/{rest}")).unwrap_or_else(|_| target.to_string()),
        None => target.to_string(),
    };
    std::thread::spawn(move || {
        let looks_like_path = target.contains("://") || target.starts_with('/');
        let ok = |args: &[&str]| Command::new("open").args(args).status().is_ok_and(|s| s.success());
        if !(looks_like_path && ok(&[&target]) || !looks_like_path && (ok(&["-a", &target]) || ok(&[&target]))) {
            log::warn!("could not open {target:?}");
        }
    });
}

// --- smooth scroll ------------------------------------------------------------

/// Distance still to scroll, in pixels: (vertical, horizontal).
struct ScrollState {
    left: Mutex<(f64, f64)>,
    wake: Condvar,
}

/// Time constant of the ease-out; ~99% of a step is done after ~5 tau.
const SCROLL_TAU: f64 = 0.025;
const SCROLL_FRAME: Duration = Duration::from_millis(8);

fn smooth_scroll(axis: Axis, pixels: i32) {
    static STATE: OnceLock<&'static ScrollState> = OnceLock::new();
    let state = STATE.get_or_init(|| {
        let state: &'static ScrollState = Box::leak(Box::new(ScrollState { left: Mutex::new((0.0, 0.0)), wake: Condvar::new() }));
        let spawned = std::thread::Builder::new().name("knob-scroll".into()).spawn(move || scroll_loop(state));
        if let Err(e) = spawned {
            log::error!("scroll thread: {e}");
        }
        state
    });
    let mut left = state.left.lock().unwrap_or_else(|e| e.into_inner());
    match axis {
        Axis::Vertical => left.0 += pixels as f64,
        Axis::Horizontal => left.1 += pixels as f64,
    }
    state.wake.notify_one();
}

fn scroll_loop(state: &ScrollState) {
    // whole pixels already sent vs. the fractional target, per axis
    let mut carry = (0.0f64, 0.0f64);
    let mut last = Instant::now();
    loop {
        let (dv, dh) = {
            let mut left = state.left.lock().unwrap_or_else(|e| e.into_inner());
            while left.0 == 0.0 && left.1 == 0.0 {
                carry = (0.0, 0.0);
                left = state.wake.wait(left).unwrap_or_else(|e| e.into_inner());
                last = Instant::now() - SCROLL_FRAME;
            }
            let dt = last.elapsed().as_secs_f64().min(0.05);
            last = Instant::now();
            let k = 1.0 - (-dt / SCROLL_TAU).exp();
            let take = |rest: &mut f64| {
                let step = if rest.abs() < 1.0 { *rest } else { *rest * k };
                *rest -= step;
                step
            };
            (take(&mut left.0), take(&mut left.1))
        };
        carry.0 += dv;
        carry.1 += dh;
        let (iv, ih) = (carry.0.trunc() as i32, carry.1.trunc() as i32);
        carry.0 -= iv as f64;
        carry.1 -= ih as f64;
        if iv != 0 || ih != 0 {
            // Quartz: positive wheel1 scrolls up, positive wheel2 scrolls left.
            let e = CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Pixel, 2, -iv, -ih, 0);
            if let Some(e) = &e {
                CGEvent::set_integer_value_field(Some(e), CGEventField::ScrollWheelEventIsContinuous, 1);
            }
            post(e);
        }
        std::thread::sleep(SCROLL_FRAME);
    }
}

/// One of the 16 steps the brightness keys use.
const BRIGHTNESS_STEP: f32 = 1.0 / 16.0;

type GetBrightness = unsafe extern "C" fn(u32, *mut f32) -> i32;
type SetBrightness = unsafe extern "C" fn(u32, f32) -> i32;

extern "C" {
    fn dlopen(path: *const std::ffi::c_char, mode: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const std::ffi::c_char) -> *mut c_void;
}

/// Synthetic brightness keys do nothing on Apple Silicon, so use the private
/// DisplayServices framework (built-in and Apple displays). False if unavailable.
fn set_brightness_by(delta: f32) -> bool {
    static FNS: OnceLock<Option<(GetBrightness, SetBrightness)>> = OnceLock::new();
    let fns = FNS.get_or_init(|| {
        // SAFETY: plain dlopen/dlsym; the symbols have these C signatures.
        unsafe {
            let h = dlopen(c"/System/Library/PrivateFrameworks/DisplayServices.framework/DisplayServices".as_ptr(), 1);
            if h.is_null() {
                return None;
            }
            let get = dlsym(h, c"DisplayServicesGetBrightness".as_ptr());
            let set = dlsym(h, c"DisplayServicesSetBrightness".as_ptr());
            if get.is_null() || set.is_null() {
                return None;
            }
            Some((std::mem::transmute::<*mut c_void, GetBrightness>(get), std::mem::transmute::<*mut c_void, SetBrightness>(set)))
        }
    });
    let Some((get, set)) = *fns else { return false };
    let display = CGMainDisplayID();
    let mut now = 0.0f32;
    // SAFETY: valid display id and out pointer.
    unsafe { get(display, &mut now) == 0 && set(display, (now + delta).clamp(0.0, 1.0)) == 0 }
}

/// A playing Spotify or Music wins; otherwise arrow keys (YouTube, most players).
fn seek(seconds: i32) {
    for (bundle, app) in [("com.spotify.client", "Spotify"), ("com.apple.Music", "Music")] {
        if !is_running(bundle) {
            continue;
        }
        let script = format!(
            "tell application \"{app}\"\n\
               if player state is playing then\n\
                 set player position to (player position + ({seconds}))\n\
                 return \"ok\"\n\
               end if\n\
             end tell"
        );
        if let Ok(out) = Command::new("osascript").arg("-e").arg(script).output() {
            if out.stdout.starts_with(b"ok") {
                return;
            }
        }
    }
    stroke(NONE, if seconds < 0 { vk::LEFT } else { vk::RIGHT });
}

fn is_running(bundle_id: &str) -> bool {
    autoreleasepool(|_| {
        let id = NSString::from_str(bundle_id);
        !NSRunningApplication::runningApplicationsWithBundleIdentifier(&id).is_empty()
    })
}

// --- queries ----------------------------------------------------------------

pub fn frontmost_app() -> Option<String> {
    autoreleasepool(|_| {
        let app = NSWorkspace::sharedWorkspace().frontmostApplication()?;
        Some(app.localizedName()?.to_string())
    })
}

pub fn accent_color() -> Option<[u8; 3]> {
    autoreleasepool(|_| {
        let c = NSColor::controlAccentColor().colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())?;
        let to8 = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        Some([to8(c.redComponent()), to8(c.greenComponent()), to8(c.blueComponent())])
    })
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
    static kAXTrustedCheckOptionPrompt: *const c_void;
}

pub fn input_permission(prompt: bool) -> bool {
    autoreleasepool(|_| {
        // SAFETY: kAXTrustedCheckOptionPrompt is a CFStringRef, toll-free bridged to NSString.
        let key: &NSString = unsafe { &*(kAXTrustedCheckOptionPrompt as *const NSString) };
        let options = NSDictionary::from_slices(&[key], &[&*NSNumber::new_bool(prompt)]);
        // SAFETY: NSDictionary is toll-free bridged to CFDictionaryRef.
        unsafe { AXIsProcessTrustedWithOptions(objc2::rc::Retained::as_ptr(&options).cast()) }
    })
}

// --- device listener (IOHIDManager) ----------------------------------------------
//
// The event tap can't tell which keyboard a media key came from. IOHIDManager
// sees the raw HID consumer report, with its device, at about the same time;
// the tap matches a key to the latest unclaimed report for the same key and
// direction, waiting briefly if the report hasn't arrived yet.

type IOHIDManagerRef = *mut c_void;
type IOHIDValueRef = *mut c_void;
type IOHIDElementRef = *mut c_void;
type IOHIDDeviceRef = *mut c_void;
type CFTypeRef = *const c_void;
type IOHIDValueCallback = unsafe extern "C" fn(*mut c_void, i32, *mut c_void, IOHIDValueRef);

const K_IOHID_REQUEST_TYPE_LISTEN_EVENT: u32 = 1;
const K_IOHID_ACCESS_TYPE_GRANTED: u32 = 0;
const CONSUMER_PAGE: u32 = 0x0C;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOHIDManagerCreate(allocator: CFTypeRef, options: u32) -> IOHIDManagerRef;
    fn IOHIDManagerSetDeviceMatching(manager: IOHIDManagerRef, matching: CFTypeRef);
    fn IOHIDManagerSetInputValueMatching(manager: IOHIDManagerRef, matching: CFTypeRef);
    fn IOHIDManagerRegisterInputValueCallback(manager: IOHIDManagerRef, callback: IOHIDValueCallback, context: *mut c_void);
    fn IOHIDManagerScheduleWithRunLoop(manager: IOHIDManagerRef, run_loop: *const CFRunLoop, mode: CFTypeRef);
    fn IOHIDManagerUnscheduleFromRunLoop(manager: IOHIDManagerRef, run_loop: *const CFRunLoop, mode: CFTypeRef);
    fn IOHIDManagerOpen(manager: IOHIDManagerRef, options: u32) -> i32;
    fn IOHIDManagerClose(manager: IOHIDManagerRef, options: u32) -> i32;
    fn IOHIDValueGetElement(value: IOHIDValueRef) -> IOHIDElementRef;
    fn IOHIDValueGetIntegerValue(value: IOHIDValueRef) -> isize;
    fn IOHIDElementGetUsagePage(element: IOHIDElementRef) -> u32;
    fn IOHIDElementGetUsage(element: IOHIDElementRef) -> u32;
    fn IOHIDElementGetDevice(element: IOHIDElementRef) -> IOHIDDeviceRef;
    fn IOHIDDeviceGetProperty(device: IOHIDDeviceRef, key: CFTypeRef) -> CFTypeRef;
    fn IOHIDCheckAccess(request: u32) -> u32;
    fn IOHIDRequestAccess(request: u32) -> bool;
}

extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFNumberGetValue(number: CFTypeRef, the_type: isize, value: *mut c_void) -> bool;
}

/// The device listener is running; the tap should look up devices.
static LISTENING: AtomicBool = AtomicBool::new(false);
/// Longest the tap waits for a HID report that hasn't arrived yet.
const REPORT_WAIT: Duration = Duration::from_millis(40);
/// Reports older than this can't belong to the key being decided.
const REPORT_MAX_AGE: Duration = Duration::from_millis(150);

struct Report {
    key: MediaKey,
    down: bool,
    at: Instant,
    device: DeviceInfo,
}

struct Reports {
    queue: Mutex<VecDeque<Report>>,
    arrived: Condvar,
}

static REPORTS: Reports = Reports { queue: Mutex::new(VecDeque::new()), arrived: Condvar::new() };
static LISTENER: Mutex<Option<Hook>> = Mutex::new(None);

fn consumer_key(usage: u32) -> Option<MediaKey> {
    Some(match usage {
        0xE9 => MediaKey::VolumeUp,
        0xEA => MediaKey::VolumeDown,
        0xE2 => MediaKey::Mute,
        0xCD => MediaKey::PlayPause,
        0xB5 => MediaKey::Next,
        0xB6 => MediaKey::Previous,
        0x6F => MediaKey::BrightnessUp,
        0x70 => MediaKey::BrightnessDown,
        _ => return None,
    })
}

/// Called by the tap for a media key while the listener runs.
fn device_for(key: MediaKey, down: bool) -> Option<DeviceInfo> {
    let start = Instant::now();
    let mut queue = REPORTS.queue.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        queue.retain(|r| r.at.elapsed() < REPORT_MAX_AGE);
        if let Some(i) = queue.iter().position(|r| r.key == key && r.down == down) {
            let report = queue.remove(i)?;
            let waited = start.elapsed();
            if waited > Duration::from_millis(1) {
                log::debug!("HID report for {key:?} arrived {waited:?} after the key");
            }
            return Some(report.device);
        }
        let left = REPORT_WAIT.saturating_sub(start.elapsed());
        if left.is_zero() {
            log::debug!("no HID report for {key:?} (down: {down}) within {REPORT_WAIT:?}");
            return None;
        }
        queue = REPORTS.arrived.wait_timeout(queue, left).unwrap_or_else(|e| e.into_inner()).0;
    }
}

unsafe extern "C" fn on_hid_value(_ctx: *mut c_void, _result: i32, _sender: *mut c_void, value: IOHIDValueRef) {
    // SAFETY: IOKit hands us a valid value for the duration of the call.
    let (element, pressed) = unsafe { (IOHIDValueGetElement(value), IOHIDValueGetIntegerValue(value) != 0) };
    if element.is_null() || unsafe { IOHIDElementGetUsagePage(element) } != CONSUMER_PAGE {
        return;
    }
    let Some(key) = consumer_key(unsafe { IOHIDElementGetUsage(element) }) else { return };
    let device = unsafe { device_info(IOHIDElementGetDevice(element)) };
    let mut queue = REPORTS.queue.lock().unwrap_or_else(|e| e.into_inner());
    queue.push_back(Report { key, down: pressed, at: Instant::now(), device });
    while queue.len() > 64 {
        queue.pop_front();
    }
    REPORTS.arrived.notify_all();
}

/// # Safety
/// `device` must be a live IOHIDDeviceRef (or null).
unsafe fn device_info(device: IOHIDDeviceRef) -> DeviceInfo {
    let mut info = DeviceInfo { vendor_id: 0, product_id: 0, name: String::new() };
    if device.is_null() {
        return info;
    }
    autoreleasepool(|_| {
        let prop = |key: &str| {
            let key = NSString::from_str(key);
            // SAFETY: NSString is toll-free bridged to CFStringRef; Get rule, not retained.
            unsafe { IOHIDDeviceGetProperty(device, objc2::rc::Retained::as_ptr(&key).cast()) }
        };
        let number = |cf: CFTypeRef| -> u32 {
            let mut n: i64 = 0;
            // SAFETY: checked to be a CFNumber; kCFNumberSInt64Type = 4.
            if !cf.is_null() && unsafe { CFGetTypeID(cf) == CFNumberGetTypeID() } {
                unsafe { CFNumberGetValue(cf, 4, (&mut n as *mut i64).cast()) };
            }
            n as u32
        };
        info.vendor_id = number(prop("VendorID"));
        info.product_id = number(prop("ProductID"));
        let name = prop("Product");
        // SAFETY: checked to be a CFString, toll-free bridged to NSString.
        if !name.is_null() && unsafe { CFGetTypeID(name) == CFStringGetTypeID() } {
            info.name = unsafe { &*(name as *const NSString) }.to_string().trim().to_string();
        }
    });
    info
}

pub fn device_permission(prompt: bool) -> bool {
    // SAFETY: plain calls with a valid request type.
    unsafe {
        if IOHIDCheckAccess(K_IOHID_REQUEST_TYPE_LISTEN_EVENT) == K_IOHID_ACCESS_TYPE_GRANTED {
            return true;
        }
        prompt && IOHIDRequestAccess(K_IOHID_REQUEST_TYPE_LISTEN_EVENT)
    }
}

pub fn set_device_listener(on: bool) -> Result<(), PlatformError> {
    let mut listener = LISTENER.lock().unwrap_or_else(|e| e.into_inner());
    if !on {
        LISTENING.store(false, Ordering::Release);
        listener.take(); // Hook's Drop stops the run loop and joins
        REPORTS.queue.lock().unwrap_or_else(|e| e.into_inner()).clear();
        return Ok(());
    }
    if listener.is_some() {
        return Ok(());
    }
    if !device_permission(false) {
        return Err(PlatformError::PermissionDenied);
    }
    let (tx, rx) = mpsc::channel::<Result<RunLoopRef, PlatformError>>();
    let thread = std::thread::Builder::new()
        .name("knob-hid".into())
        .spawn(move || run_hid_listener(tx))
        .map_err(|e| PlatformError::Other(e.to_string()))?;
    match rx.recv() {
        Ok(Ok(run_loop)) => {
            *listener = Some(Hook { run_loop, thread: Some(thread) });
            LISTENING.store(true, Ordering::Release);
            log::info!("device listener running");
            Ok(())
        }
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => Err(PlatformError::Other("device listener thread exited".into())),
    }
}

fn run_hid_listener(tx: mpsc::Sender<Result<RunLoopRef, PlatformError>>) {
    // SAFETY: standard IOHIDManager setup; every pointer passed is live.
    unsafe {
        let manager = IOHIDManagerCreate(ptr::null(), 0);
        if manager.is_null() {
            let _ = tx.send(Err(PlatformError::Other("IOHIDManagerCreate failed".into())));
            return;
        }
        // All devices (the knob's consumer controls usually sit inside its
        // keyboard device), but only consumer-page values reach the callback.
        IOHIDManagerSetDeviceMatching(manager, ptr::null());
        autoreleasepool(|_| {
            let key = NSString::from_str("UsagePage");
            let page = NSNumber::new_u32(CONSUMER_PAGE);
            let matching = NSDictionary::from_slices(&[&*key], &[&*page]);
            IOHIDManagerSetInputValueMatching(manager, objc2::rc::Retained::as_ptr(&matching).cast());
        });
        IOHIDManagerRegisterInputValueCallback(manager, on_hid_value, ptr::null_mut());
        let run_loop = CFRunLoop::current().expect("thread has a run loop");
        let mode: CFTypeRef = kCFRunLoopCommonModes.map_or(ptr::null(), |m| (m as *const objc2_core_foundation::CFString).cast());
        IOHIDManagerScheduleWithRunLoop(manager, CFRetained::as_ptr(&run_loop).as_ptr(), mode);
        // Some devices (e.g. ones Karabiner seized) refuse to open; the rest still work.
        let status = IOHIDManagerOpen(manager, 0);
        if status != 0 {
            log::debug!("IOHIDManagerOpen: {status:#x} (some devices not opened)");
        }
        let _ = tx.send(Ok(RunLoopRef(run_loop.clone())));

        CFRunLoop::run();

        IOHIDManagerUnscheduleFromRunLoop(manager, CFRetained::as_ptr(&run_loop).as_ptr(), mode);
        IOHIDManagerClose(manager, 0);
        CFRelease(manager);
    }
}
