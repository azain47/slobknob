//! Wires the hook to the engine. Three threads:
//! - hook thread: asks the engine for a decision, queues effects, returns;
//! - timer thread: sleeps until the engine's next deadline and ticks it;
//! - worker thread: performs actions and forwards UI / mode events.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Instant;

use knob_core::{Action, Config, Decision, Effect, Engine, InputEvent, KnobKey, LearnEvent, MediaKey, UiEvent};

use crate::{device_permission, frontmost_app, perform, set_device_listener, start_hook, Hook, HookEvent, PlatformError};

pub type UiCallback = Box<dyn Fn(UiEvent) + Send>;
/// (wheel key, mode id) after a mode change; persist it.
pub type ModeCallback = Box<dyn Fn(&str, &str) + Send>;
pub type LearnCallback = Box<dyn Fn(LearnEvent) + Send>;
/// (knob key, down) while the preview is on.
pub type PreviewCallback = Box<dyn Fn(KnobKey, bool) + Send>;

pub struct Callbacks {
    pub on_ui: UiCallback,
    pub on_mode: ModeCallback,
    pub on_learn: LearnCallback,
    pub on_preview: PreviewCallback,
}

enum Msg {
    Effect(Effect),
    Stop,
}

struct Shared {
    engine: Mutex<Engine>,
    /// Wakes the timer thread when deadlines may have changed.
    wake: Condvar,
    tx: Mutex<mpsc::Sender<Msg>>,
    last_app: Mutex<Option<String>>,
    stopping: AtomicBool,
    /// Whether the platform's device listener is currently on.
    listening: Mutex<bool>,
}

impl Shared {
    fn engine(&self) -> MutexGuard<'_, Engine> {
        self.engine.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn send(&self, fx: Vec<Effect>) {
        if fx.is_empty() {
            return;
        }
        let tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        for f in fx {
            let _ = tx.send(Msg::Effect(f));
        }
    }

    /// Turn the device listener on while learning or while the device filter
    /// needs it, and off otherwise.
    fn update_listener(&self) {
        let want = {
            let e = self.engine();
            let cfg = e.config();
            e.is_learning() || (cfg.only_knob_device && cfg.knob_device.is_some())
        };
        let mut on = self.listening.lock().unwrap_or_else(|e| e.into_inner());
        if *on == want {
            return;
        }
        if want && !device_permission(false) {
            log::warn!("device listener needs Input Monitoring permission");
            return;
        }
        match set_device_listener(want) {
            Ok(()) => *on = want,
            Err(e) => log::warn!("device listener: {e}"),
        }
    }

    /// Run `f` on the engine, then queue its effects and wake the timer.
    fn with<R>(&self, f: impl FnOnce(&mut Engine) -> (R, Vec<Effect>)) -> R {
        let (r, fx) = f(&mut self.engine());
        self.send(fx);
        self.wake.notify_one();
        r
    }
}

pub struct Runtime {
    shared: Arc<Shared>,
    hook: Option<Hook>,
    timer: Option<JoinHandle<()>>,
    worker: Option<JoinHandle<()>>,
}

impl Runtime {
    pub fn start(
        cfg: Config,
        saved_modes: std::collections::HashMap<String, String>,
        callbacks: Callbacks,
    ) -> Result<Runtime, PlatformError> {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            engine: Mutex::new(Engine::new(cfg, saved_modes)),
            wake: Condvar::new(),
            tx: Mutex::new(tx),
            last_app: Mutex::new(None),
            stopping: AtomicBool::new(false),
            listening: Mutex::new(false),
        });

        let worker_shared = shared.clone();
        let worker = std::thread::Builder::new()
            .name("knob-worker".into())
            .spawn(move || {
                for msg in rx {
                    match msg {
                        Msg::Stop => break,
                        Msg::Effect(Effect::Perform(a)) => {
                            log::debug!("perform {a:?}");
                            perform(&a);
                        }
                        Msg::Effect(Effect::Ui(u)) => (callbacks.on_ui)(u),
                        Msg::Effect(Effect::ModeChanged { wheel, mode }) => (callbacks.on_mode)(&wheel, &mode),
                        Msg::Effect(Effect::Learn(l)) => {
                            if !matches!(l, LearnEvent::Prompt { .. }) {
                                worker_shared.update_listener();
                            }
                            (callbacks.on_learn)(l);
                        }
                        Msg::Effect(Effect::Preview { key, down }) => (callbacks.on_preview)(key, down),
                    }
                }
            })
            .map_err(|e| PlatformError::Other(e.to_string()))?;

        let timer_shared = shared.clone();
        let timer = std::thread::Builder::new()
            .name("knob-timer".into())
            .spawn(move || timer_loop(&timer_shared))
            .map_err(|e| PlatformError::Other(e.to_string()))?;

        let hook_shared = shared.clone();
        let hook = start_hook(Box::new(move |ev| on_hook_event(&hook_shared, ev)));

        let mut rt = Runtime { shared, hook: None, timer: Some(timer), worker: Some(worker) };
        rt.hook = Some(hook?); // on error, Drop stops the threads
        rt.shared.update_listener();
        Ok(rt)
    }

    /// "Learn my knob". Progress arrives through `on_learn`.
    pub fn start_learn(&self) {
        self.shared.with(|e| ((), e.start_learn(Instant::now())));
        self.shared.update_listener();
    }

    pub fn cancel_learn(&self) {
        self.shared.with(|e| ((), e.cancel_learn()));
        self.shared.update_listener();
    }

    /// While on, knob keys only reach `on_preview`. Turn it off whenever the
    /// settings window loses focus or closes, or the knob does nothing.
    pub fn set_preview(&self, on: bool) {
        self.shared.with(|e| ((), e.set_preview(on)));
    }

    pub fn cancel_wheel(&self) {
        self.shared.with(|e| ((), e.cancel_wheel()));
    }

    /// A media key that reached the app some other way than the hook (Windows:
    /// a hotkey while the settings window is in front). Runs it as the hook
    /// would. The key itself is already consumed, so "pass" is carried out as
    /// an action instead.
    pub fn feed_key(&self, key: MediaKey, down: bool, modifier: bool) {
        let ev = InputEvent { key, down, repeat: false, modifier, device: None };
        if on_hook_event(&self.shared, HookEvent::Key(ev)) == Decision::Pass && down {
            self.shared.send(vec![Effect::Perform(Action::Media(key))]);
        }
    }

    /// Pick a mode for the wheel of `front_app` (tray menu).
    pub fn set_mode_for_app(&self, front_app: Option<&str>, mode: &str) {
        self.shared.with(|e| {
            let key = e.wheel_key_for(front_app);
            ((), e.set_mode(&key, mode))
        });
    }

    pub fn set_config(&self, cfg: Config) {
        self.shared.with(|e| ((), e.set_config(cfg)));
        self.shared.update_listener();
    }

    /// Read-only access to the engine, e.g. for building the tray menu.
    pub fn inspect<R>(&self, f: impl FnOnce(&Engine) -> R) -> R {
        f(&self.shared.engine())
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.hook.take();
        if std::mem::take(&mut *self.shared.listening.lock().unwrap_or_else(|e| e.into_inner())) {
            let _ = set_device_listener(false);
        }
        self.shared.stopping.store(true, Ordering::Release);
        self.shared.wake.notify_one();
        if let Some(t) = self.timer.take() {
            let _ = t.join();
        }
        let _ = self.shared.tx.lock().unwrap_or_else(|e| e.into_inner()).send(Msg::Stop);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

fn on_hook_event(shared: &Shared, ev: HookEvent) -> Decision {
    match ev {
        HookEvent::Key(input) => {
            let now = Instant::now();
            let app = frontmost_app();
            shared.with(|e| {
                let mut fx = Vec::new();
                let mut last = shared.last_app.lock().unwrap_or_else(|e| e.into_inner());
                if *last != app {
                    if last.is_some() {
                        fx.extend(e.app_changed());
                    }
                    last.clone_from(&app);
                }
                let (decision, more) = e.handle_input(&input, now, app.as_deref());
                fx.extend(more);
                (decision, fx)
            })
        }
        HookEvent::ModifierReleased => {
            shared.with(|e| ((), e.modifier_released(Instant::now())));
            Decision::Pass
        }
    }
}

fn timer_loop(shared: &Shared) {
    let mut engine = shared.engine();
    while !shared.stopping.load(Ordering::Acquire) {
        let fx = engine.tick(Instant::now());
        shared.send(fx);
        engine = match engine.next_deadline() {
            Some(t) => {
                let wait = t.saturating_duration_since(Instant::now());
                shared.wake.wait_timeout(engine, wait).unwrap_or_else(|e| e.into_inner()).0
            }
            None => shared.wake.wait(engine).unwrap_or_else(|e| e.into_inner()),
        };
    }
}
