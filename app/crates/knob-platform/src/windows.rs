//! Windows: a `WH_KEYBOARD_LL` hook for media keys, `SendInput` for actions,
//! and an optional Raw Input listener that tells which HID device sent each
//! media key (the approach of github.com/gatikash/knob-control).
//!
//! With the listener off, the hook asks the handler and returns its decision
//! at once. With it on, the hook can't know the device yet: WM_INPUT may
//! arrive before or after the hook sees the key. So the hook swallows every
//! media key and queues it. The resolver thread pairs each key with a raw
//! report (or gives up after [`MATCH_WAIT`]), asks the handler, and
//! re-injects the key if the answer is Pass. The queue is strictly FIFO.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::mem::offset_of;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering::SeqCst};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use knob_core::{Axis, DeviceInfo, Key, KeyCombo, MediaKey, Mods, NamedKey};
use windows::core::{w, BOOL, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Devices::HumanInterfaceDevice::{
    HidD_GetProductString, HidP_GetUsages, HidP_Input, HIDP_STATUS_INCOMPATIBLE_REPORT_ID,
    HIDP_STATUS_REPORT_DOES_NOT_EXIST, HIDP_STATUS_SUCCESS, HIDP_STATUS_USAGE_NOT_FOUND, PHIDP_PREPARSED_DATA,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Dwm::DwmGetColorizationColor;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW, CREATE_NO_WINDOW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Input::{
    GetRawInputData, GetRawInputDeviceInfoW, GetRawInputDeviceList, RegisterRawInputDevices, HRAWINPUT, RAWHID,
    RAWINPUT, RAWINPUTDEVICE, RAWINPUTDEVICELIST, RAWINPUTHEADER, RIDEV_INPUTSINK, RIDEV_REMOVE, RIDI_DEVICEINFO, RIDI_DEVICENAME, RIDI_PREPARSEDDATA,
    RID_DEVICE_INFO, RID_DEVICE_INFO_HID, RID_INPUT, RIM_TYPEHID,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DestroyWindow, DispatchMessageW, GetCursorPos, GetForegroundWindow,
    GetMessageW, GetSystemMetrics, GetWindowThreadProcessId, PeekMessageW, PostThreadMessageW, SetWindowsHookExW,
    UnhookWindowsHookEx, HC_ACTION, HWND_MESSAGE, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, PM_NOREMOVE,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_SHOWNORMAL, WHEEL_DELTA,
    WH_KEYBOARD_LL, WINDOW_EX_STYLE, WINDOW_STYLE, WM_INPUT, WM_KEYDOWN, WM_QUIT, WM_SYSKEYDOWN, WM_USER,
};

use crate::{Action, Decision, HookEvent, InputEvent, KeyHandler, PlatformError};

/// `dwExtraInfo` of every event we inject ("KNOB"), so our hook skips them.
const MAGIC: usize = 0x4B4E_4F42;

/// Unassigned key tapped while Win is held to stop Start from opening on
/// release (AutoHotkey's default menu mask key).
const VK_MASK: VIRTUAL_KEY = VIRTUAL_KEY(0xE8);

/// How long a queued key waits for its raw report (knob-control uses 80 ms).
const MATCH_WAIT: Duration = Duration::from_millis(80);
/// How long a raw report that no key claimed stays claimable.
const REPORT_TTL: Duration = Duration::from_millis(250);

// --- media keys ------------------------------------------------------------

fn media_key(vk: VIRTUAL_KEY) -> Option<MediaKey> {
    Some(match vk {
        VK_VOLUME_UP => MediaKey::VolumeUp,
        VK_VOLUME_DOWN => MediaKey::VolumeDown,
        VK_VOLUME_MUTE => MediaKey::Mute,
        VK_MEDIA_PLAY_PAUSE => MediaKey::PlayPause,
        VK_MEDIA_NEXT_TRACK => MediaKey::Next,
        VK_MEDIA_PREV_TRACK => MediaKey::Previous,
        _ => return None,
    })
}

fn media_vk(key: MediaKey) -> Option<VIRTUAL_KEY> {
    Some(match key {
        MediaKey::VolumeUp => VK_VOLUME_UP,
        MediaKey::VolumeDown => VK_VOLUME_DOWN,
        MediaKey::Mute => VK_VOLUME_MUTE,
        MediaKey::PlayPause => VK_MEDIA_PLAY_PAUSE,
        MediaKey::Next => VK_MEDIA_NEXT_TRACK,
        MediaKey::Previous => VK_MEDIA_PREV_TRACK,
        MediaKey::BrightnessUp | MediaKey::BrightnessDown => return None,
    })
}

/// Small index per media key, for bit sets and per-key arrays.
fn media_index(key: MediaKey) -> usize {
    match key {
        MediaKey::VolumeUp => 0,
        MediaKey::VolumeDown => 1,
        MediaKey::Mute => 2,
        MediaKey::PlayPause => 3,
        MediaKey::Next => 4,
        MediaKey::Previous => 5,
        MediaKey::BrightnessUp => 6,
        MediaKey::BrightnessDown => 7,
    }
}

/// HID Consumer page usage to media key.
fn usage_key(usage: u16) -> Option<MediaKey> {
    Some(match usage {
        0xE9 => MediaKey::VolumeUp,
        0xEA => MediaKey::VolumeDown,
        0xE2 => MediaKey::Mute,
        0xCD => MediaKey::PlayPause,
        0xB5 => MediaKey::Next,
        0xB6 => MediaKey::Previous,
        _ => return None,
    })
}

// --- hook ------------------------------------------------------------------

static HOOK_ACTIVE: AtomicBool = AtomicBool::new(false);
/// The Raw Input listener is running, so the hook queues media keys.
static LISTENING: AtomicBool = AtomicBool::new(false);
/// Events queued for the resolver and not fully handled yet. While non-zero
/// the hook keeps queueing even with the listener off, so a later key (say a
/// key-up) can never overtake one still in the queue.
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
/// The resolver's inbox, for the listener's reports. Set while the hook runs.
static RESOLVER: Mutex<Option<mpsc::Sender<ResolverMsg>>> = Mutex::new(None);

thread_local! {
    /// Lives on the hook thread: a low-level hook is called on the thread that
    /// installed it, so the callback needs no lock. Only borrowed mutably
    /// before the hook is installed and after it is removed.
    static STATE: RefCell<Option<HookState>> = const { RefCell::new(None) };
}

struct HookState {
    /// Shared with the resolver thread.
    handler: Arc<KeyHandler>,
    /// Wakes the injector thread to tap [`VK_MASK`].
    mask: mpsc::Sender<()>,
    /// Queue to the resolver thread.
    resolver: mpsc::Sender<ResolverMsg>,
    /// Physically held Win keys: bit 0 left, bit 1 right.
    win: Cell<u8>,
    /// The last Win event was physical, so the async key state agrees with
    /// `win`. Injected Win presses (our DesktopStep, other tools) break that.
    win_async_ok: Cell<bool>,
    /// The mask key was already sent during this Win hold.
    start_masked: Cell<bool>,
    /// Media keys currently down (bit per [`media_index`]), for `repeat`.
    keys: Cell<u8>,
}

impl HookState {
    fn on_key(&self, kb: &KBDLLHOOKSTRUCT, msg: u32) -> Decision {
        let vk = VIRTUAL_KEY(kb.vkCode as u16);
        let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let injected = kb.flags.contains(LLKHF_INJECTED);
        let Some(key) = media_key(vk) else {
            return self.on_win(vk, down, injected);
        };
        if kb.dwExtraInfo == MAGIC {
            return Decision::Pass;
        }
        self.on_media(key, down, injected)
    }

    fn on_win(&self, vk: VIRTUAL_KEY, down: bool, injected: bool) -> Decision {
        if injected {
            self.win_async_ok.set(false);
            return Decision::Pass;
        }
        self.win_async_ok.set(true);
        if down {
            // Closes out a hold whose key-up was lost, so its mask flag resets.
            // The async state doesn't include this press yet.
            self.win_held();
        }
        let bit = if vk == VK_LWIN { 1 } else { 2 };
        // Auto-repeat of a held Win after the mask went out: Windows re-arms
        // Start on any Win down that follows the mask, so drop the repeats.
        // The key stays down; only the repeats go missing.
        if down && self.win.get() & bit != 0 && self.start_masked.get() {
            return Decision::Swallow;
        }
        let held = if down { self.win.get() | bit } else { self.win.get() & !bit };
        self.win.set(held);
        if !down && held == 0 {
            self.modifier_released();
        }
        Decision::Pass
    }

    fn on_media(&self, key: MediaKey, down: bool, injected: bool) -> Decision {
        let bit = 1u8 << media_index(key);
        let keys = self.keys.get();
        self.keys.set(if down { keys | bit } else { keys & !bit });
        // May queue a ModifierReleased first, which then stays ahead of this key.
        let modifier = self.win_held();
        let ev = InputEvent { key, down, repeat: down && keys & bit != 0, modifier, device: None };
        let decision = if self.queueing() {
            match self.queue(Queued::Key { ev, injected, at: Instant::now() }) {
                Ok(()) => Decision::Swallow,
                Err(Queued::Key { ev, .. }) => crate::call_handler(&self.handler, HookEvent::Key(ev)),
                Err(Queued::ModifierReleased) => Decision::Pass,
            }
        } else {
            crate::call_handler(&self.handler, HookEvent::Key(ev))
        };
        // Windows opens Start when Win goes up with no other key seen during
        // the hold, and a swallowed key doesn't count. Queued keys are always
        // swallowed here, so mask now; a later re-injection doesn't hurt.
        // Never SendInput from here: injected input is routed through this
        // very callback.
        if decision == Decision::Swallow && modifier && !self.start_masked.replace(true) {
            let _ = self.mask.send(());
        }
        decision
    }

    fn win_held(&self) -> bool {
        // A Win key-up can go missing, e.g. Win+L switches to the secure
        // desktop where hooks don't run. Trust the system state then.
        if self.win.get() != 0 && self.win_async_ok.get() && !async_down(VK_LWIN) && !async_down(VK_RWIN) {
            self.win.set(0);
            self.modifier_released();
        }
        self.win.get() != 0
    }

    fn modifier_released(&self) {
        self.start_masked.set(false);
        // Goes through the queue when keys are queued, so the engine never
        // sees the release before keys pressed during the hold.
        if !self.queueing() || self.queue(Queued::ModifierReleased).is_err() {
            crate::call_handler(&self.handler, HookEvent::ModifierReleased);
        }
    }

    /// Whether events must go through the resolver. Two atomic loads, so the
    /// listener-off path stays immediate.
    fn queueing(&self) -> bool {
        LISTENING.load(SeqCst) || IN_FLIGHT.load(SeqCst) > 0
    }

    /// Hands the event to the resolver; gives it back if the resolver is gone.
    fn queue(&self, q: Queued) -> Result<(), Queued> {
        IN_FLIGHT.fetch_add(1, SeqCst);
        self.resolver.send(ResolverMsg::Hook(q)).map_err(|mpsc::SendError(msg)| {
            IN_FLIGHT.fetch_sub(1, SeqCst);
            log::error!("the key resolver thread is gone; deciding in the hook");
            match msg {
                ResolverMsg::Hook(q) => q,
                _ => Queued::ModifierReleased,
            }
        })
    }
}

fn async_down(vk: VIRTUAL_KEY) -> bool {
    // SAFETY: no preconditions. The high bit (sign) means "down".
    unsafe { GetAsyncKeyState(i32::from(vk.0)) < 0 }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for HC_ACTION, lparam points to a KBDLLHOOKSTRUCT valid for this call.
        let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        // This sees all typing: anything but Win and the media keys leaves at once.
        let vk = VIRTUAL_KEY(kb.vkCode as u16);
        let ours = matches!(vk, VK_LWIN | VK_RWIN) || media_key(vk).is_some();
        if ours {
            // try_* so nothing can panic across the FFI boundary.
            let decision = STATE
                .try_with(|s| match s.try_borrow().as_deref() {
                    Ok(Some(state)) => state.on_key(kb, wparam.0 as u32),
                    _ => Decision::Pass,
                })
                .unwrap_or(Decision::Pass);
            if decision == Decision::Swallow {
                return LRESULT(1);
            }
        }
    }
    // SAFETY: forwarding the arguments we were called with.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// The installed hook. Dropping it removes the hook and stops its threads.
pub struct Hook {
    thread_id: u32,
    hook_thread: Option<JoinHandle<()>>,
    injector: Option<JoinHandle<()>>,
    resolver_tx: mpsc::Sender<ResolverMsg>,
    resolver: Option<JoinHandle<()>>,
}

impl Drop for Hook {
    fn drop(&mut self) {
        // SAFETY: no preconditions; the hook thread made its queue before reporting ready.
        let posted = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        if let Err(e) = posted {
            if !self.hook_thread.as_ref().is_some_and(JoinHandle::is_finished) {
                // Joining would hang; leak the thread rather than the caller.
                log::error!("could not stop the keyboard hook thread: {e}");
                return;
            }
        }
        // The injector ends once the hook thread drops its sender.
        if let Some(t) = self.hook_thread.take() {
            let _ = t.join();
        }
        // The resolver handles what is still queued, without waiting, then ends.
        *lock(&RESOLVER) = None;
        let _ = self.resolver_tx.send(ResolverMsg::Stop);
        for t in [self.resolver.take(), self.injector.take()].into_iter().flatten() {
            let _ = t.join();
        }
        IN_FLIGHT.store(0, SeqCst);
        HOOK_ACTIVE.store(false, SeqCst);
    }
}

pub fn start_hook(handler: KeyHandler) -> Result<Hook, PlatformError> {
    if HOOK_ACTIVE.swap(true, SeqCst) {
        return Err(PlatformError::Other("the keyboard hook is already running".into()));
    }
    spawn_hook(handler).inspect_err(|_| HOOK_ACTIVE.store(false, SeqCst))
}

fn spawn_hook(handler: KeyHandler) -> Result<Hook, PlatformError> {
    let spawn_err = |e: std::io::Error| PlatformError::Other(format!("could not start a thread: {e}"));
    let handler = Arc::new(handler);

    let (mask, mask_rx) = mpsc::channel::<()>();
    let injector = thread::Builder::new()
        .name("knob-inject".into())
        .spawn(move || {
            for () in mask_rx {
                send(&[key(VK_MASK, false), key(VK_MASK, true)]);
            }
        })
        .map_err(spawn_err)?;

    let (resolver_tx, resolver_rx) = mpsc::channel::<ResolverMsg>();
    let resolver_handler = handler.clone();
    let resolver = match thread::Builder::new()
        .name("knob-resolve".into())
        .spawn(move || run_resolver(&resolver_handler, &resolver_rx))
    {
        Ok(t) => t,
        Err(e) => {
            drop(mask);
            let _ = injector.join();
            return Err(spawn_err(e));
        }
    };
    let stop_resolver = |resolver: JoinHandle<()>| {
        let _ = resolver_tx.send(ResolverMsg::Stop);
        let _ = resolver.join();
    };

    let state = HookState {
        handler,
        mask,
        resolver: resolver_tx.clone(),
        win: Cell::new(0),
        win_async_ok: Cell::new(true),
        start_masked: Cell::new(false),
        keys: Cell::new(0),
    };
    let (ready_tx, ready_rx) = mpsc::channel();
    let hook_thread = match thread::Builder::new().name("knob-hook".into()).spawn(move || run_hook(state, ready_tx)) {
        Ok(t) => t,
        Err(e) => {
            // The unspawned closure dropped the mask sender; the injector exits.
            let _ = injector.join();
            stop_resolver(resolver);
            return Err(spawn_err(e));
        }
    };

    let ready = ready_rx.recv().unwrap_or_else(|_| Err(PlatformError::Other("the keyboard hook thread died".into())));
    match ready {
        Ok(thread_id) => {
            *lock(&RESOLVER) = Some(resolver_tx.clone());
            Ok(Hook {
                thread_id,
                hook_thread: Some(hook_thread),
                injector: Some(injector),
                resolver_tx,
                resolver: Some(resolver),
            })
        }
        Err(e) => {
            let _ = hook_thread.join();
            let _ = injector.join();
            stop_resolver(resolver);
            Err(e)
        }
    }
}

fn run_hook(state: HookState, ready: mpsc::Sender<Result<u32, PlatformError>>) {
    STATE.with(|s| *s.borrow_mut() = Some(state));
    let mut msg = MSG::default();
    // SAFETY: msg is a valid out pointer and hook_proc has the HOOKPROC signature.
    let hook = unsafe {
        // Creates this thread's message queue so Hook::drop can post WM_QUIT.
        let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
        let module = GetModuleHandleW(None).ok().map(HINSTANCE::from);
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), module, 0)
    };
    match hook {
        Ok(hook) => {
            // SAFETY: no preconditions.
            let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));
            // hook_proc runs inside GetMessageW. 0 is WM_QUIT, -1 an error;
            // there are no windows to dispatch to.
            // SAFETY: msg is a valid out pointer.
            while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {}
            // SAFETY: hook came from SetWindowsHookExW on this thread.
            if let Err(e) = unsafe { UnhookWindowsHookEx(hook) } {
                log::warn!("UnhookWindowsHookEx failed: {e}");
            }
        }
        Err(e) => {
            let _ = ready.send(Err(PlatformError::Other(format!("SetWindowsHookExW failed: {e}"))));
        }
    }
    // Drop the handler and the senders now, not in TLS teardown.
    drop(STATE.with(|s| s.borrow_mut().take()));
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// --- resolver --------------------------------------------------------------

enum ResolverMsg {
    /// From the hook, in hook order.
    Hook(Queued),
    /// From the Raw Input listener.
    Report(Report),
    /// Handle what is queued without waiting, then exit.
    Stop,
}

#[derive(Clone)]
enum Queued {
    Key {
        ev: InputEvent,
        /// Injected by some other program: no raw report will come.
        injected: bool,
        at: Instant,
    },
    ModifierReleased,
}

/// A media key press or release seen in a HID report.
struct Report {
    key: MediaKey,
    down: bool,
    device: DeviceInfo,
    at: Instant,
}

struct Resolver<'a> {
    handler: &'a KeyHandler,
    pending: VecDeque<Queued>,
    reports: VecDeque<Report>,
    /// Per media key: the device of its last queued (non-repeat) down, until
    /// its up. Outer None: no queued down seen.
    down_device: [Option<Option<DeviceInfo>>; 8],
    /// Per media key: whether its last queued down was re-injected. None:
    /// unknown (the down went through the synchronous path).
    reinjected: [Option<bool>; 8],
}

fn run_resolver(handler: &KeyHandler, rx: &mpsc::Receiver<ResolverMsg>) {
    let mut r = Resolver {
        handler,
        pending: VecDeque::new(),
        reports: VecDeque::new(),
        down_device: Default::default(),
        reinjected: [None; 8],
    };
    loop {
        let now = Instant::now();
        r.expire(now);
        while r.step(now, false) {}
        let msg = match r.deadline() {
            Some(t) => match rx.recv_timeout(t.saturating_duration_since(now)) {
                Ok(m) => m,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => ResolverMsg::Stop,
            },
            None => rx.recv().unwrap_or(ResolverMsg::Stop),
        };
        match msg {
            ResolverMsg::Hook(q) => r.pending.push_back(q),
            ResolverMsg::Report(rep) => r.reports.push_back(rep),
            ResolverMsg::Stop => {
                while r.step(Instant::now(), true) {}
                return;
            }
        }
    }
}

impl Resolver<'_> {
    /// Drops reports too old to belong to any queued key.
    fn expire(&mut self, now: Instant) {
        let oldest = self
            .pending
            .iter()
            .find_map(|q| match q {
                Queued::Key { at, .. } => Some(*at),
                Queued::ModifierReleased => None,
            })
            .unwrap_or(now)
            .min(now);
        self.reports.retain(|r| r.at + REPORT_TTL >= oldest);
    }

    /// When the head of the queue stops waiting for its report.
    fn deadline(&self) -> Option<Instant> {
        match self.pending.front()? {
            Queued::Key { at, .. } => Some(*at + MATCH_WAIT),
            Queued::ModifierReleased => Some(Instant::now()),
        }
    }

    /// Takes the earliest unclaimed report for this key and direction.
    fn take_report(&mut self, key: MediaKey, down: bool, at: Instant) -> Option<DeviceInfo> {
        let i = self.reports.iter().position(|r| r.key == key && r.down == down && r.at + REPORT_TTL >= at)?;
        self.reports.remove(i).map(|r| r.device)
    }

    /// Handles the head of the queue if it can be handled now (or `force`).
    /// Returns whether it did.
    fn step(&mut self, now: Instant, force: bool) -> bool {
        let Some(head) = self.pending.front().cloned() else { return false };
        match head {
            Queued::ModifierReleased => {
                self.pending.pop_front();
                crate::call_handler(self.handler, HookEvent::ModifierReleased);
            }
            Queued::Key { ev, injected, at } => {
                let i = media_index(ev.key);
                let give_up = force || injected || !LISTENING.load(SeqCst) || now >= at + MATCH_WAIT;
                let device = match (ev.down && !ev.repeat, self.down_device[i].clone()) {
                    // Repeats have no report; ups take the device of their down.
                    (false, Some(device)) => {
                        if !ev.down {
                            let _ = self.take_report(ev.key, false, at);
                        }
                        device
                    }
                    (false, None) if ev.repeat => None,
                    _ => match self.take_report(ev.key, ev.down, at) {
                        Some(device) => Some(device),
                        None if give_up => None,
                        None => return false,
                    },
                };
                self.pending.pop_front();
                self.deliver(InputEvent { device, ..ev });
            }
        }
        IN_FLIGHT.fetch_sub(1, SeqCst);
        true
    }

    fn deliver(&mut self, ev: InputEvent) {
        let i = media_index(ev.key);
        let (key, down, repeat) = (ev.key, ev.down, ev.repeat);
        let device = ev.device.clone();
        let pass = crate::call_handler(self.handler, HookEvent::Key(ev)) == Decision::Pass;
        let reinject = if down {
            if !repeat {
                self.down_device[i] = Some(device);
                self.reinjected[i] = Some(pass);
            } else if pass {
                self.reinjected[i] = Some(true);
            }
            pass
        } else {
            // Never a lone up for a swallowed down, and never leave a
            // re-injected down without its up.
            let reinject = self.reinjected[i].unwrap_or(pass);
            self.down_device[i] = None;
            self.reinjected[i] = None;
            reinject
        };
        if reinject {
            if let Some(vk) = media_vk(key) {
                send(&[self::key(vk, !down)]);
            }
        }
    }
}

// --- device listener (Raw Input) -------------------------------------------

struct Listener {
    thread_id: u32,
    thread: JoinHandle<()>,
}

static LISTENER: Mutex<Option<Listener>> = Mutex::new(None);

pub fn set_device_listener(on: bool) -> Result<(), PlatformError> {
    let mut slot = lock(&LISTENER);
    if on {
        if slot.is_some() {
            return Ok(());
        }
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("knob-rawinput".into())
            .spawn(move || run_listener(&ready_tx))
            .map_err(|e| PlatformError::Other(format!("could not start the raw input thread: {e}")))?;
        match ready_rx.recv() {
            Ok(Ok(thread_id)) => {
                *slot = Some(Listener { thread_id, thread });
                LISTENING.store(true, SeqCst);
                log::info!("device listener on");
                Ok(())
            }
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(PlatformError::Other("the raw input thread died".into()))
            }
        }
    } else {
        let Some(listener) = slot.take() else { return Ok(()) };
        LISTENING.store(false, SeqCst);
        // SAFETY: no preconditions; the thread made its queue before reporting ready.
        let posted = unsafe { PostThreadMessageW(listener.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        match posted {
            Err(e) if !listener.thread.is_finished() => {
                // Joining would hang; leak the thread rather than the caller.
                log::error!("could not stop the raw input thread: {e}");
            }
            _ => {
                let _ = listener.thread.join();
                log::info!("device listener off");
            }
        }
        Ok(())
    }
}

/// Raw Input needs no permission on Windows.
pub fn device_permission(_prompt: bool) -> bool {
    true
}

/// Consumer Control top-level collection.
fn consumer_device(flags: windows::Win32::UI::Input::RAWINPUTDEVICE_FLAGS, target: HWND) -> RAWINPUTDEVICE {
    RAWINPUTDEVICE { usUsagePage: 0x0C, usUsage: 0x01, dwFlags: flags, hwndTarget: target }
}

fn run_listener(ready: &mpsc::Sender<Result<u32, PlatformError>>) {
    let mut msg = MSG::default();
    // SAFETY: msg is a valid out pointer. Creates this thread's message queue
    // so set_device_listener(false) can post WM_QUIT.
    let _ = unsafe { PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE) };
    // A message-only window of the built-in STATIC class: no class to
    // register, and its window procedure (DefWindowProc for WM_INPUT) frees
    // the raw input buffer.
    // SAFETY: all arguments are valid; the window is destroyed below on this thread.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("SlobKnob raw input"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
    };
    let hwnd = match hwnd {
        Ok(h) => h,
        Err(e) => {
            let _ = ready.send(Err(PlatformError::Other(format!("could not create the raw input window: {e}"))));
            return;
        }
    };
    let device = consumer_device(RIDEV_INPUTSINK, hwnd);
    // SAFETY: device is fully initialized; hwnd is our window.
    if let Err(e) = unsafe { RegisterRawInputDevices(&[device], size_of::<RAWINPUTDEVICE>() as u32) } {
        let _ = ready.send(Err(PlatformError::Other(format!("RegisterRawInputDevices failed: {e}"))));
        // SAFETY: hwnd was created on this thread.
        let _ = unsafe { DestroyWindow(hwnd) };
        return;
    }
    // SAFETY: no preconditions.
    let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));

    let mut devices = known_consumer_devices();
    // SAFETY: msg is a valid out pointer. 0 is WM_QUIT, -1 an error.
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        if msg.message == WM_INPUT {
            read_raw_input(HRAWINPUT(msg.lParam.0 as _), &mut devices);
        }
        // SAFETY: msg came from GetMessageW.
        unsafe { DispatchMessageW(&msg) };
    }

    let remove = consumer_device(RIDEV_REMOVE, HWND::default());
    // SAFETY: removing a registration needs a null target window.
    if let Err(e) = unsafe { RegisterRawInputDevices(&[remove], size_of::<RAWINPUTDEVICE>() as u32) } {
        log::warn!("could not unregister raw input: {e}");
    }
    // SAFETY: hwnd was created on this thread.
    let _ = unsafe { DestroyWindow(hwnd) };
}

/// Looks up the Consumer Control devices already attached, so the first
/// report from each doesn't wait on the (possibly slow) lookup and miss
/// [`MATCH_WAIT`]. Devices attached later are looked up on first report.
fn known_consumer_devices() -> HashMap<isize, HidDevice> {
    let mut devices = HashMap::new();
    let entry_size = size_of::<RAWINPUTDEVICELIST>() as u32;
    let mut count = 0u32;
    // SAFETY: a null buffer asks for the count.
    if unsafe { GetRawInputDeviceList(None, &mut count, entry_size) } == u32::MAX || count == 0 {
        return devices;
    }
    // A little slack in case a device arrives between the two calls.
    let mut list = vec![RAWINPUTDEVICELIST::default(); count as usize + 4];
    let mut cap = list.len() as u32;
    // SAFETY: list holds cap entries.
    let got = unsafe { GetRawInputDeviceList(Some(list.as_mut_ptr()), &mut cap, entry_size) };
    if got == u32::MAX {
        return devices;
    }
    for entry in list.iter().take(got as usize).filter(|e| e.dwType == RIM_TYPEHID) {
        let consumer = device_hid_info(entry.hDevice).is_some_and(|h| h.usUsagePage == 0x0C && h.usUsage == 0x01);
        if consumer {
            devices.insert(entry.hDevice.0 as isize, HidDevice::open(entry.hDevice));
        }
    }
    devices
}

fn read_raw_input(handle: HRAWINPUT, devices: &mut HashMap<isize, HidDevice>) {
    let now = Instant::now();
    let header_size = size_of::<RAWINPUTHEADER>() as u32;
    let mut size = 0u32;
    // SAFETY: a null buffer asks for the size.
    if unsafe { GetRawInputData(handle, RID_INPUT, None, &mut size, header_size) } != 0 || size == 0 {
        return;
    }
    // u64s so the RAWINPUT header is suitably aligned.
    let mut buf = vec![0u64; (size as usize).div_ceil(8)];
    // SAFETY: buf holds at least `size` bytes.
    let got = unsafe { GetRawInputData(handle, RID_INPUT, Some(buf.as_mut_ptr().cast()), &mut size, header_size) };
    let data_at = offset_of!(RAWINPUT, data);
    let raw_at = data_at + offset_of!(RAWHID, bRawData);
    if got == u32::MAX || got as usize > buf.len() * 8 || (got as usize) < raw_at {
        return;
    }
    // SAFETY: buf is initialized and holds `got` bytes.
    let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), got as usize) };
    // SAFETY: buf is 8-byte aligned and holds at least a header.
    let header = unsafe { &*buf.as_ptr().cast::<RAWINPUTHEADER>() };
    if header.dwType != RIM_TYPEHID.0 {
        return;
    }
    let u32_at = |at: usize| bytes.get(at..at + 4).map(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]));
    let (Some(size_hid), Some(count)) = (u32_at(data_at), u32_at(data_at + 4)) else { return };
    let size_hid = size_hid as usize;
    let data = &bytes[raw_at..];
    let Some(total) = size_hid.checked_mul(count as usize).filter(|&t| size_hid > 0 && t <= data.len()) else {
        return;
    };
    let device_handle = header.hDevice;
    let device = devices.entry(device_handle.0 as isize).or_insert_with(|| HidDevice::open(device_handle));
    for report in data[..total].chunks_exact(size_hid) {
        device.on_report(report, now);
    }
}

/// A HID device seen by the listener, looked up once per handle.
struct HidDevice {
    info: DeviceInfo,
    /// HidP preparsed data (u64s for alignment), if the device gave it.
    preparsed: Option<Vec<u64>>,
    /// Our media keys currently down, per report id.
    pressed: HashMap<u8, Vec<MediaKey>>,
}

impl HidDevice {
    fn open(handle: HANDLE) -> HidDevice {
        let path = device_name(handle);
        let (mut vendor_id, mut product_id) = path.as_deref().and_then(parse_vid_pid).unwrap_or((0, 0));
        if let Some(hid) = device_hid_info(handle) {
            if hid.dwVendorId != 0 || hid.dwProductId != 0 {
                (vendor_id, product_id) = (hid.dwVendorId, hid.dwProductId);
            }
        }
        let name = path
            .as_deref()
            .and_then(product_string)
            .unwrap_or_else(|| format!("HID device {vendor_id:04X}:{product_id:04X}"));
        let preparsed = preparsed_data(handle);
        log::info!(
            "raw input device \"{name}\" ({vendor_id:04x}:{product_id:04x}, {}): {}",
            if preparsed.is_some() { "HidP" } else { "byte parse" },
            path.as_deref().unwrap_or("?")
        );
        HidDevice { info: DeviceInfo { vendor_id, product_id, name }, preparsed, pressed: HashMap::new() }
    }

    /// Diffs the media keys in this report against the last one with the
    /// same report id: new ones are presses, missing ones releases (a
    /// consumer release report is an empty array).
    fn on_report(&mut self, report: &[u8], now: Instant) {
        let Some(&id) = report.first() else { return };
        let Some(keys) = self.media_keys(report) else { return };
        let old = self.pressed.insert(id, keys.clone()).unwrap_or_default();
        let presses = keys.iter().filter(|k| !old.contains(k)).map(|&k| (k, true));
        let releases = old.iter().filter(|k| !keys.contains(k)).map(|&k| (k, false));
        let changes: Vec<(MediaKey, bool)> = releases.chain(presses).collect();
        if changes.is_empty() {
            return;
        }
        let Some(tx) = lock(&RESOLVER).clone() else { return };
        for (key, down) in changes {
            log::trace!("raw input: {key:?} {} from {}", if down { "down" } else { "up" }, self.info.name);
            let _ = tx.send(ResolverMsg::Report(Report { key, down, device: self.info.clone(), at: now }));
        }
    }

    /// The media keys held in this report, or None if it carries no
    /// consumer controls.
    fn media_keys(&self, report: &[u8]) -> Option<Vec<MediaKey>> {
        let mut keys = match &self.preparsed {
            Some(pp) => {
                let mut usages = [0u16; 64];
                let mut len = usages.len() as u32;
                let mut copy = report.to_vec();
                // SAFETY: usages holds len u16s; pp is this device's preparsed data.
                let status = unsafe {
                    HidP_GetUsages(
                        HidP_Input,
                        0x0C,
                        None,
                        usages.as_mut_ptr(),
                        &mut len,
                        PHIDP_PREPARSED_DATA(pp.as_ptr() as isize),
                        &mut copy,
                    )
                };
                if status == HIDP_STATUS_SUCCESS {
                    usages.iter().take(len as usize).filter_map(|&u| usage_key(u)).collect()
                } else if [HIDP_STATUS_INCOMPATIBLE_REPORT_ID, HIDP_STATUS_USAGE_NOT_FOUND, HIDP_STATUS_REPORT_DOES_NOT_EXIST]
                    .contains(&status)
                {
                    return None;
                } else {
                    parse_report_bytes(report)
                }
            }
            None => parse_report_bytes(report),
        };
        keys.sort_by_key(|&k| media_index(k));
        keys.dedup();
        Some(keys)
    }
}

/// Best effort without HidP: after the report id, an array of 16-bit
/// little-endian usages (or one 8-bit usage). Bitmap-style reports only
/// decode correctly through HidP.
fn parse_report_bytes(report: &[u8]) -> Vec<MediaKey> {
    let body = report.get(1..).unwrap_or_default();
    if let [usage] = body {
        return usage_key(u16::from(*usage)).into_iter().collect();
    }
    body.as_chunks::<2>().0.iter().filter_map(|&pair| usage_key(u16::from_le_bytes(pair))).collect()
}

fn device_name(handle: HANDLE) -> Option<String> {
    let mut len = 0u32;
    // SAFETY: a null buffer asks for the length in characters.
    unsafe { GetRawInputDeviceInfoW(Some(handle), RIDI_DEVICENAME, None, &mut len) };
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u16; len as usize + 1];
    let mut cap = buf.len() as u32;
    // SAFETY: buf holds cap u16s.
    let got = unsafe { GetRawInputDeviceInfoW(Some(handle), RIDI_DEVICENAME, Some(buf.as_mut_ptr().cast()), &mut cap) };
    if got == u32::MAX || got == 0 {
        return None;
    }
    let text = buf.get(..(got as usize).min(buf.len()))?;
    let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
    Some(String::from_utf16_lossy(&text[..end]))
}

fn device_hid_info(handle: HANDLE) -> Option<RID_DEVICE_INFO_HID> {
    let mut info = RID_DEVICE_INFO { cbSize: size_of::<RID_DEVICE_INFO>() as u32, ..Default::default() };
    let mut size = info.cbSize;
    // SAFETY: info is a RID_DEVICE_INFO with cbSize set; size is its size.
    let got = unsafe {
        GetRawInputDeviceInfoW(Some(handle), RIDI_DEVICEINFO, Some((&raw mut info).cast()), &mut size)
    };
    if got == u32::MAX || got == 0 || info.dwType != RIM_TYPEHID {
        return None;
    }
    // SAFETY: dwType says the hid variant is the one filled in.
    Some(unsafe { info.Anonymous.hid })
}

fn preparsed_data(handle: HANDLE) -> Option<Vec<u64>> {
    let mut size = 0u32;
    // SAFETY: a null buffer asks for the size in bytes.
    unsafe { GetRawInputDeviceInfoW(Some(handle), RIDI_PREPARSEDDATA, None, &mut size) };
    if size == 0 {
        return None;
    }
    let mut buf = vec![0u64; (size as usize).div_ceil(8)];
    let mut cap = (buf.len() * 8) as u32;
    // SAFETY: buf holds cap bytes.
    let got =
        unsafe { GetRawInputDeviceInfoW(Some(handle), RIDI_PREPARSEDDATA, Some(buf.as_mut_ptr().cast()), &mut cap) };
    (got != u32::MAX && got != 0).then_some(buf)
}

/// The USB product string, read through the device path.
fn product_string(path: &str) -> Option<String> {
    let path = HSTRING::from(path);
    // SAFETY: path is a valid wide string. Access 0 only queries attributes,
    // which works even for keyboards the system holds open.
    let file = unsafe {
        CreateFileW(
            &path,
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .ok()?;
    let mut buf = [0u16; 128];
    // SAFETY: buf holds size_of_val(&buf) bytes.
    let ok = unsafe { HidD_GetProductString(file, buf.as_mut_ptr().cast(), size_of_val(&buf) as u32) };
    // SAFETY: file is an open handle we own.
    let _ = unsafe { CloseHandle(file) };
    if !ok {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let name = String::from_utf16_lossy(&buf[..end]).trim().to_owned();
    (!name.is_empty()).then_some(name)
}

/// VID/PID from a device path: `HID#VID_046D&PID_C52B...` (USB) or
/// `..._VID&0002046d_PID&b023...` (Bluetooth).
fn parse_vid_pid(path: &str) -> Option<(u32, u32)> {
    let path = path.to_ascii_uppercase();
    let hex_after = |tag: &str, digits: usize| {
        let start = path.find(tag)? + tag.len();
        let hex = path.get(start..start + digits)?;
        if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        u32::from_str_radix(hex, 16).ok()
    };
    let vid = hex_after("VID_", 4).or_else(|| hex_after("VID&", 8).map(|v| v & 0xFFFF))?;
    let pid = hex_after("PID_", 4).or_else(|| hex_after("PID&", 4))?;
    Some((vid, pid))
}

// --- actions ---------------------------------------------------------------

struct Held {
    /// We sent Alt down for the app switcher.
    alt: bool,
    drag: Option<Drag>,
}

struct Drag {
    origin: POINT,
    pos: POINT,
}

static HELD: Mutex<Held> = Mutex::new(Held { alt: false, drag: None });

fn held() -> MutexGuard<'static, Held> {
    lock(&HELD)
}

pub fn perform(action: &Action) {
    match action {
        Action::Media(media) => match media_vk(*media) {
            Some(vk) => tap(&[], vk),
            None => brightness_step(if *media == MediaKey::BrightnessUp { 1 } else { -1 }),
        },
        Action::Scroll { axis, lines } => scroll(*axis, *lines),
        Action::SmoothScroll { axis, pixels } => smooth_scroll(*axis, *pixels),
        Action::ZoomIn => tap(&[VK_CONTROL], VK_OEM_PLUS),
        Action::ZoomOut => tap(&[VK_CONTROL], VK_OEM_MINUS),
        Action::ZoomReset => tap(&[VK_CONTROL], VK_0),
        Action::AppSwitchStep(dir) => app_switch_step(*dir),
        Action::AppSwitchEnd => app_switch_end(),
        Action::WindowCycle(_) => log::debug!("WindowCycle has no Windows equivalent; ignored"),
        Action::TabCycle(dir) => tap(if *dir < 0 { &[VK_CONTROL, VK_SHIFT] } else { &[VK_CONTROL] }, VK_TAB),
        Action::DesktopStep(dir) => tap(&[VK_LWIN, VK_CONTROL], if *dir < 0 { VK_LEFT } else { VK_RIGHT }),
        Action::DesktopOverview => tap(&[VK_LWIN], VK_TAB),
        Action::Seek { seconds } => tap(&[], if *seconds > 0 { VK_RIGHT } else { VK_LEFT }),
        Action::DragStep { axis, pixels } => drag_step(*axis, *pixels),
        Action::DragEnd => drag_end(),
        Action::FrameStep(dir) => tap(&[], if *dir < 0 { VK_LEFT } else { VK_RIGHT }),
        Action::EditPoint(dir) => tap(&[], if *dir < 0 { VK_UP } else { VK_DOWN }),
        Action::SpaceKey => tap(&[], VK_SPACE),
        Action::UndoRedo(dir) => tap(&[VK_CONTROL], if *dir < 0 { VK_Z } else { VK_Y }),
        Action::Shortcut(combos) => shortcut(combos),
        Action::Open(target) => open(target),
        Action::TypeText(text) => type_text(text),
    }
}

/// Presses `mods` in order, taps `vk`, releases `mods` in reverse.
fn chord(mods: &[VIRTUAL_KEY], vk: VIRTUAL_KEY) -> Vec<INPUT> {
    let mut inputs = Vec::with_capacity(mods.len() * 2 + 2);
    inputs.extend(mods.iter().map(|&m| key(m, false)));
    inputs.push(key(vk, false));
    inputs.push(key(vk, true));
    inputs.extend(mods.iter().rev().map(|&m| key(m, true)));
    inputs
}

fn tap(mods: &[VIRTUAL_KEY], vk: VIRTUAL_KEY) {
    send(&chord(mods, vk));
}

fn shortcut(combos: &[KeyCombo]) {
    for combo in combos {
        let Some(vk) = key_vk(combo.key) else {
            log::warn!("shortcut key {:?} has no Windows key code; skipped", combo.key);
            continue;
        };
        let mut mods = Vec::with_capacity(4);
        if combo.mods.contains(Mods::CTRL) || combo.mods.contains(Mods::PRIMARY) {
            mods.push(VK_CONTROL);
        }
        if combo.mods.contains(Mods::ALT) {
            mods.push(VK_MENU);
        }
        if combo.mods.contains(Mods::SHIFT) {
            mods.push(VK_SHIFT);
        }
        let win = combo.mods.contains(Mods::META);
        if win {
            mods.push(VK_LWIN);
        }
        let mut inputs = Vec::with_capacity(mods.len() * 2 + 4);
        inputs.extend(mods.iter().map(|&m| key(m, false)));
        inputs.push(key(vk, false));
        inputs.push(key(vk, true));
        if win {
            // Keeps Start from opening when Win goes up.
            inputs.push(key(VK_MASK, false));
            inputs.push(key(VK_MASK, true));
        }
        inputs.extend(mods.iter().rev().map(|&m| key(m, true)));
        send(&inputs);
    }
}

/// US-layout virtual key for a shortcut key.
fn key_vk(key: Key) -> Option<VIRTUAL_KEY> {
    Some(match key {
        Key::Char(c) if c.is_ascii_alphanumeric() => VIRTUAL_KEY(u16::from(c.to_ascii_uppercase() as u8)),
        Key::Char(c) => match c {
            '`' => VK_OEM_3,
            '-' => VK_OEM_MINUS,
            '=' => VK_OEM_PLUS,
            '[' => VK_OEM_4,
            ']' => VK_OEM_6,
            '\\' => VK_OEM_5,
            ';' => VK_OEM_1,
            '\'' => VK_OEM_7,
            ',' => VK_OEM_COMMA,
            '.' => VK_OEM_PERIOD,
            '/' => VK_OEM_2,
            _ => return None,
        },
        Key::Named(named) => match named {
            NamedKey::Enter => VK_RETURN,
            NamedKey::Tab => VK_TAB,
            NamedKey::Space => VK_SPACE,
            NamedKey::Escape => VK_ESCAPE,
            NamedKey::Backspace => VK_BACK,
            NamedKey::Delete => VK_DELETE,
            NamedKey::Up => VK_UP,
            NamedKey::Down => VK_DOWN,
            NamedKey::Left => VK_LEFT,
            NamedKey::Right => VK_RIGHT,
            NamedKey::Home => VK_HOME,
            NamedKey::End => VK_END,
            NamedKey::PageUp => VK_PRIOR,
            NamedKey::PageDown => VK_NEXT,
            NamedKey::F(n @ 1..=24) => VIRTUAL_KEY(VK_F1.0 + u16::from(n) - 1),
            NamedKey::F(_) => return None,
        },
    })
}

/// Types `text` as Unicode key events; newlines and tabs as Enter and Tab.
fn type_text(text: &str) {
    /// Characters per SendInput call.
    const BATCH: usize = 64;
    let mut inputs: Vec<INPUT> = Vec::with_capacity(BATCH * 4);
    let mut chars = text.chars().peekable();
    let mut count = 0;
    while let Some(c) = chars.next() {
        match c {
            '\r' if chars.peek() == Some(&'\n') => continue,
            '\r' | '\n' => inputs.extend(chord(&[], VK_RETURN)),
            '\t' => inputs.extend(chord(&[], VK_TAB)),
            _ => {
                let mut units = [0u16; 2];
                for &unit in c.encode_utf16(&mut units).iter() {
                    inputs.push(unicode_key(unit, false));
                    inputs.push(unicode_key(unit, true));
                }
            }
        }
        count += 1;
        // Flushing only between characters keeps surrogate pairs together.
        if count % BATCH == 0 {
            send(&inputs);
            inputs.clear();
        }
    }
    if !inputs.is_empty() {
        send(&inputs);
    }
}

fn unicode_key(unit: u16, up: bool) -> INPUT {
    let flags = if up { KEYEVENTF_UNICODE | KEYEVENTF_KEYUP } else { KEYEVENTF_UNICODE };
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0), wScan: unit, dwFlags: flags, time: 0, dwExtraInfo: MAGIC },
        },
    }
}

/// Opens a URL, a path (a leading `~` is the user profile) or an app by
/// name (ShellExecute resolves registered App Paths like "chrome"). Runs on
/// its own short thread: ShellExecute can block (DDE, network paths).
fn open(target: &str) {
    let target = target.trim();
    if target.is_empty() {
        log::warn!("Open: empty target");
        return;
    }
    let target = expand_home(target);
    let spawned = thread::Builder::new().name("knob-open".into()).spawn(move || shell_open(&target));
    if let Err(e) = spawned {
        log::warn!("could not start the open thread: {e}");
    }
}

fn expand_home(target: &str) -> String {
    let Some(rest) = target.strip_prefix('~') else { return target.to_owned() };
    if !(rest.is_empty() || rest.starts_with(['/', '\\'])) {
        return target.to_owned();
    }
    let Some(home) = std::env::var_os("USERPROFILE") else { return target.to_owned() };
    let mut path = PathBuf::from(home);
    path.extend(rest.split(['/', '\\']).filter(|p| !p.is_empty()));
    path.to_string_lossy().into_owned()
}

fn shell_open(target: &str) {
    // ShellExecute wants COM initialized for some targets.
    // SAFETY: no preconditions; balanced below when it succeeds.
    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    let file = HSTRING::from(target);
    // SAFETY: the strings are valid null-terminated wide strings for the call.
    let result = unsafe { ShellExecuteW(None, w!("open"), &file, PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
    // Values above 32 mean success; the rest are SE_ERR_* codes.
    let code = result.0 as isize;
    if code <= 32 {
        let why = match code {
            0 => "out of memory",
            2 => "file not found",
            3 => "path not found",
            5 => "access denied",
            31 => "no app is associated with it",
            _ => "ShellExecute failed",
        };
        log::warn!("could not open {target:?}: {why} (code {code})");
    }
    if com.is_ok() {
        // SAFETY: balances the successful CoInitializeEx on this thread.
        unsafe { CoUninitialize() };
    }
}

fn app_switch_step(dir: i8) {
    let mut held = held();
    let mut inputs = Vec::with_capacity(5);
    if !held.alt {
        inputs.push(key(VK_MENU, false));
        held.alt = true;
    }
    inputs.extend(chord(if dir < 0 { &[VK_SHIFT] } else { &[] }, VK_TAB));
    send(&inputs);
}

fn app_switch_end() {
    if std::mem::take(&mut held().alt) {
        send(&[key(VK_MENU, true)]);
    }
}

fn scroll(axis: Axis, lines: i32) {
    send_wheel(axis, lines.saturating_mul(WHEEL_DELTA as i32));
}

/// Sends `units` of wheel (120 per notch); positive is down or right.
fn send_wheel(axis: Axis, units: i32) {
    let (flags, data) = match axis {
        // The vertical wheel is positive away from the user (up).
        Axis::Vertical => (MOUSEEVENTF_WHEEL, units.saturating_neg()),
        Axis::Horizontal => (MOUSEEVENTF_HWHEEL, units),
    };
    // mouseData is a DWORD holding a signed value.
    send(&[mouse(flags, 0, 0, data as u32)]);
}

// --- smooth scroll ---------------------------------------------------------

/// Wheel units per pixel: 40 px is one notch of 120.
const UNITS_PER_PX: f64 = WHEEL_DELTA as f64 / 40.0;
/// About how long a scroll takes to ease out.
const SCROLL_EASE: Duration = Duration::from_millis(120);
/// About 120 Hz.
const SCROLL_FRAME: Duration = Duration::from_micros(8_333);

struct ScrollState {
    /// Pixels still to scroll, [vertical, horizontal]; positive down / right.
    remaining: [f64; 2],
    /// Wheel units computed but not sent yet (always under one unit).
    carry: [f64; 2],
}

static SCROLL: Mutex<ScrollState> = Mutex::new(ScrollState { remaining: [0.0; 2], carry: [0.0; 2] });
static SCROLL_WAKE: Condvar = Condvar::new();
/// Whether the animator thread started.
static SCROLL_THREAD: OnceLock<bool> = OnceLock::new();

const AXES: [Axis; 2] = [Axis::Vertical, Axis::Horizontal];

fn axis_index(axis: Axis) -> usize {
    match axis {
        Axis::Vertical => 0,
        Axis::Horizontal => 1,
    }
}

/// Adds to the distance the animator thread still has to scroll. Never blocks.
fn smooth_scroll(axis: Axis, pixels: i32) {
    if pixels == 0 {
        return;
    }
    let running = *SCROLL_THREAD.get_or_init(|| {
        thread::Builder::new()
            .name("knob-scroll".into())
            .spawn(scroll_loop)
            .inspect_err(|e| log::warn!("could not start the scroll thread: {e}"))
            .is_ok()
    });
    if !running {
        send_wheel(axis, (f64::from(pixels) * UNITS_PER_PX).round() as i32);
        return;
    }
    lock(&SCROLL).remaining[axis_index(axis)] += f64::from(pixels);
    SCROLL_WAKE.notify_one();
}

fn scroll_loop() {
    let mut st = lock(&SCROLL);
    loop {
        while st.remaining == [0.0; 2] {
            st = SCROLL_WAKE.wait(st).unwrap_or_else(PoisonError::into_inner);
        }
        let mut last = Instant::now();
        while st.remaining != [0.0; 2] {
            drop(st);
            thread::sleep(SCROLL_FRAME);
            let now = Instant::now();
            let dt = now.duration_since(last).as_secs_f64();
            last = now;
            st = lock(&SCROLL);
            // Exponential approach, from the real frame time so a late frame
            // doesn't slow it: ~98% of the distance after SCROLL_EASE.
            let k = 1.0 - (-4.0 * dt / SCROLL_EASE.as_secs_f64()).exp();
            let mut out = [0i32; 2];
            for (i, units_out) in out.iter_mut().enumerate() {
                let rem = st.remaining[i];
                let step = if rem.abs() <= 1.0 { rem } else { rem * k };
                st.remaining[i] = rem - step;
                let units = step * UNITS_PER_PX + st.carry[i];
                let whole = units.trunc();
                st.carry[i] = units - whole;
                *units_out = whole as i32;
            }
            drop(st);
            for (axis, units) in AXES.into_iter().zip(out) {
                if units != 0 {
                    send_wheel(axis, units);
                }
            }
            st = lock(&SCROLL);
        }
    }
}

// --- drag ------------------------------------------------------------------

fn drag_step(axis: Axis, pixels: i32) {
    let mut held = held();
    let mut inputs = Vec::with_capacity(2);
    if held.drag.is_none() {
        let mut origin = POINT::default();
        // SAFETY: origin is a valid out pointer.
        if let Err(e) = unsafe { GetCursorPos(&mut origin) } {
            log::warn!("GetCursorPos failed: {e}");
            return;
        }
        inputs.push(mouse(MOUSEEVENTF_LEFTDOWN, 0, 0, 0));
        held.drag = Some(Drag { origin, pos: origin });
    }
    let Some(drag) = held.drag.as_mut() else { return };
    match axis {
        Axis::Horizontal => drag.pos.x = drag.pos.x.saturating_add(pixels),
        Axis::Vertical => drag.pos.y = drag.pos.y.saturating_sub(pixels),
    }
    inputs.push(move_to(drag.pos));
    send(&inputs);
}

fn drag_end() {
    let Some(drag) = held().drag.take() else { return };
    send(&[mouse(MOUSEEVENTF_LEFTUP, 0, 0, 0), move_to(drag.origin)]);
}

/// Absolute pointer move to `p` in virtual-screen pixels. Unlike
/// SetCursorPos, which takes effect at once, this is queued in order with the
/// button events around it, so a press or release can't land at the wrong spot.
fn move_to(p: POINT) -> INPUT {
    // SAFETY: no preconditions.
    let metric = |m| unsafe { GetSystemMetrics(m) };
    // Pixel center, in 0..=65535 across the virtual desktop.
    let norm = |v: i32, start: i32, size: i32| {
        let n = ((i64::from(v) - i64::from(start)) * 65536 + 32768) / i64::from(size.max(1));
        n.clamp(0, 65535) as i32
    };
    let dx = norm(p.x, metric(SM_XVIRTUALSCREEN), metric(SM_CXVIRTUALSCREEN));
    let dy = norm(p.y, metric(SM_YVIRTUALSCREEN), metric(SM_CYVIRTUALSCREEN));
    mouse(MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK, dx, dy, 0)
}

// --- input helpers ---------------------------------------------------------

fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    let mut flags = if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) };
    if is_extended(vk) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    // Some apps read the scan code; 0 when the layout has none.
    // SAFETY: no preconditions.
    let scan = unsafe { MapVirtualKeyW(u32::from(vk.0), MAPVK_VK_TO_VSC) } as u16;
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: MAGIC } },
    }
}

/// Keys whose scan code has the E0 prefix.
fn is_extended(vk: VIRTUAL_KEY) -> bool {
    matches!(
        vk,
        VK_LEFT
            | VK_RIGHT
            | VK_UP
            | VK_DOWN
            | VK_HOME
            | VK_END
            | VK_PRIOR
            | VK_NEXT
            | VK_INSERT
            | VK_DELETE
            | VK_LWIN
            | VK_RWIN
            | VK_VOLUME_MUTE
            | VK_VOLUME_DOWN
            | VK_VOLUME_UP
            | VK_MEDIA_PLAY_PAUSE
            | VK_MEDIA_NEXT_TRACK
            | VK_MEDIA_PREV_TRACK
    )
}

fn mouse(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: MAGIC } },
    }
}

fn send(inputs: &[INPUT]) {
    // SAFETY: every INPUT is fully initialized and cbsize is its size.
    let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        // UIPI (input into an elevated window) fails without an error code.
        let err = windows::core::Error::from_thread();
        log::warn!("SendInput inserted {sent} of {} events: {err}", inputs.len());
    }
}

// --- brightness ------------------------------------------------------------

/// Brightness steps not yet applied.
static BRIGHTNESS_PENDING: AtomicI32 = AtomicI32::new(0);
/// A PowerShell job is running and will pick up pending steps.
static BRIGHTNESS_BUSY: AtomicBool = AtomicBool::new(false);

/// Changes brightness by `steps` × 10 via WMI (built-in panels only). Each
/// PowerShell run takes about a second, so steps that arrive meanwhile are
/// applied together by the next run.
fn brightness_step(steps: i32) {
    BRIGHTNESS_PENDING.fetch_add(steps, SeqCst);
    if BRIGHTNESS_BUSY.swap(true, SeqCst) {
        return;
    }
    let spawned = thread::Builder::new().name("knob-brightness".into()).spawn(|| loop {
        let steps = BRIGHTNESS_PENDING.swap(0, SeqCst);
        if steps != 0 {
            change_brightness(steps.saturating_mul(10));
        }
        BRIGHTNESS_BUSY.store(false, SeqCst);
        // A step that landed after the swap saw BUSY set and left it to us.
        if BRIGHTNESS_PENDING.load(SeqCst) == 0 || BRIGHTNESS_BUSY.swap(true, SeqCst) {
            break;
        }
    });
    if let Err(e) = spawned {
        BRIGHTNESS_BUSY.store(false, SeqCst);
        log::warn!("could not start the brightness thread: {e}");
    }
}

fn change_brightness(delta: i32) {
    let script = format!(
        "$m = Get-CimInstance -Namespace root/wmi -ClassName WmiMonitorBrightness -ErrorAction Stop \
           | Select-Object -First 1; \
         if (-not $m) {{ exit 1 }}; \
         $b = [Math]::Max(0, [Math]::Min(100, [int]$m.CurrentBrightness + ({delta}))); \
         Get-CimInstance -Namespace root/wmi -ClassName WmiMonitorBrightnessMethods -ErrorAction Stop \
           | Invoke-CimMethod -MethodName WmiSetBrightness \
               -Arguments @{{ Timeout = [uint32]0; Brightness = [byte]$b }} | Out-Null"
    );
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW.0)
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            log::warn!("brightness change failed ({}): {}", out.status, String::from_utf8_lossy(&out.stderr).trim())
        }
        Err(e) => log::warn!("could not run powershell: {e}"),
    }
}

// --- queries ---------------------------------------------------------------

pub fn frontmost_app() -> Option<String> {
    let mut pid = 0u32;
    // SAFETY: pid is a valid out pointer; a null window yields pid 0.
    unsafe { GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid as *mut u32)) };
    if pid == 0 {
        return None;
    }
    // SAFETY: no preconditions; the handle is closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: buf holds len u16s; len is in/out.
    let queried = unsafe { QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) };
    // SAFETY: process is an open handle we own.
    let _ = unsafe { CloseHandle(process) };
    queried.ok()?;
    let path = String::from_utf16_lossy(buf.get(..len as usize)?);
    Path::new(&path).file_stem().map(|s| s.to_string_lossy().into_owned())
}

pub fn accent_color() -> Option<[u8; 3]> {
    let mut argb = 0u32;
    let mut opaque = BOOL(0);
    // SAFETY: both are valid out pointers.
    unsafe { DwmGetColorizationColor(&mut argb, &mut opaque) }.ok()?;
    let [_, r, g, b] = argb.to_be_bytes();
    Some([r, g, b])
}

/// Low-level hooks need no permission on Windows.
pub fn input_permission(_prompt: bool) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vid_pid() {
        assert_eq!(parse_vid_pid(r"\\?\HID#VID_046D&PID_C52B&MI_01&Col02#7&1&0&0001#{guid}"), Some((0x046D, 0xC52B)));
        assert_eq!(
            parse_vid_pid(r"\\?\HID#{00001124-0000-1000-8000-00805f9b34fb}_VID&0002046d_PID&b023&Col02#8&0#{guid}"),
            Some((0x046D, 0xB023))
        );
        assert_eq!(parse_vid_pid(r"\\?\HID#ACPI0C50&Col01#4&0#{guid}"), None);
    }

    #[test]
    fn parses_report_bytes() {
        assert_eq!(parse_report_bytes(&[0x02, 0xE9, 0x00]), vec![MediaKey::VolumeUp]);
        assert_eq!(parse_report_bytes(&[0x02, 0x00, 0x00]), vec![]);
        assert_eq!(parse_report_bytes(&[0x01, 0xCD]), vec![MediaKey::PlayPause]);
        assert_eq!(parse_report_bytes(&[0x03, 0xEA, 0x00, 0xE2, 0x00]), vec![MediaKey::VolumeDown, MediaKey::Mute]);
    }

    #[test]
    fn maps_shortcut_keys() {
        assert_eq!(key_vk(Key::Char('a')), Some(VK_A));
        assert_eq!(key_vk(Key::Char('7')), Some(VK_7));
        assert_eq!(key_vk(Key::Char('=')), Some(VK_OEM_PLUS));
        assert_eq!(key_vk(Key::Named(NamedKey::F(1))), Some(VK_F1));
        assert_eq!(key_vk(Key::Named(NamedKey::F(20))), Some(VK_F20));
        assert_eq!(key_vk(Key::Named(NamedKey::PageDown)), Some(VK_NEXT));
    }
}
