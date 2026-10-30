use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::config::{ActionKind, Config, CustomAction, SliceDef, Trigger};
use crate::keys::KeyCombo;
use crate::modes::mode_def;
use crate::types::{
    Action, Axis, Decision, DeviceInfo, Effect, InputEvent, KnobEvent, KnobKey, KnobKeys, LearnEvent, MediaKey,
    OpenVia, Slice, SliceKind, UiEvent,
};

/// "Learn my knob" gives up after this long without progress.
const LEARN_TIMEOUT_MS: u64 = 20_000;
/// Scroll turns closer together than this get the full acceleration...
const ACCEL_FAST_MS: f32 = 30.0;
/// ...and turns this far apart get none.
const ACCEL_SLOW_MS: f32 = 200.0;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// Clears `slot` and returns true if it holds a deadline at or before `now`.
fn take_due(slot: &mut Option<Instant>, now: Instant) -> bool {
    if slot.is_some_and(|t| t <= now) {
        *slot = None;
        true
    } else {
        false
    }
}

struct OpenWheel {
    /// Wheel key the picked mode is saved under (the parent's for sub-wheels).
    key: String,
    list: Vec<String>,
    sel: usize,
    start_sel: usize,
    via: OpenVia,
    /// A tap picks the highlight (opened by double-tap or modifier, or
    /// released after a hold without turning).
    sticky: bool,
    rotated_while_held: bool,
}

#[derive(Default)]
struct Deadlines {
    /// Double-tap trigger: a single tap is waiting to see if a second follows.
    tap: Option<Instant>,
    /// Hold trigger: open the wheel if still pressed.
    hold: Option<Instant>,
    confirm: Option<Instant>,
    wheel_timeout: Option<Instant>,
    app_switch: Option<Instant>,
    drag: Option<Instant>,
    revert: Option<Instant>,
    learn: Option<Instant>,
}

impl Deadlines {
    fn all(&self) -> [Option<Instant>; 8] {
        [self.tap, self.hold, self.confirm, self.wheel_timeout, self.app_switch, self.drag, self.revert, self.learn]
    }
}

struct Learn {
    step: KnobKey,
    keys: Vec<MediaKey>,
    device: Option<DeviceInfo>,
}

/// The knob state machine. Not thread-safe by itself; wrap it in a mutex and
/// keep calls short, since the hook thread waits on [`Engine::handle`].
pub struct Engine {
    cfg: Config,
    /// Selected mode per wheel key.
    modes: HashMap<String, String>,
    wheel: Option<OpenWheel>,
    pressing: bool,
    swallow_next_up: bool,
    /// Wheel key at the time of the last press, for actions that fire later.
    press_wheel_key: String,
    revert_wheel_key: String,
    /// Rotation key-ups get the same decision as their key-down.
    passing: HashMap<KnobKey, bool>,
    scroll_axis: Axis,
    drag_axis: Axis,
    app_switch_active: bool,
    drag_active: bool,
    /// Last scroll turn, for acceleration.
    last_scroll: Option<(Instant, i8)>,
    learn: Option<Learn>,
    /// Settings window in front: knob turns only move its preview.
    preview: bool,
    deadlines: Deadlines,
}

impl Engine {
    pub fn new(cfg: Config, saved_modes: HashMap<String, String>) -> Self {
        Self {
            cfg: cfg.sanitized(),
            modes: saved_modes,
            wheel: None,
            pressing: false,
            swallow_next_up: false,
            press_wheel_key: String::new(),
            revert_wheel_key: String::new(),
            passing: HashMap::new(),
            scroll_axis: Axis::Vertical,
            drag_axis: Axis::Horizontal,
            app_switch_active: false,
            drag_active: false,
            last_scroll: None,
            learn: None,
            preview: false,
            deadlines: Deadlines::default(),
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Replace the config (e.g. after the file changed). Closes an open wheel.
    pub fn set_config(&mut self, cfg: Config) -> Vec<Effect> {
        let mut fx = Vec::new();
        self.close_wheel(&mut fx);
        self.release_held(&mut fx);
        self.cfg = cfg.sanitized();
        fx
    }

    /// Selected mode per wheel key, for persisting.
    pub fn saved_modes(&self) -> &HashMap<String, String> {
        &self.modes
    }

    pub fn is_wheel_open(&self) -> bool {
        self.wheel.is_some()
    }

    pub fn wheel_key_for(&self, front_app: Option<&str>) -> String {
        self.cfg.wheel_key_for(front_app).to_string()
    }

    /// Mode in use on a wheel. Any known mode counts, even one not on the
    /// wheel (picked from the tray); otherwise the wheel's first mode.
    pub fn current_mode(&self, wheel_key: &str) -> &str {
        match self.modes.get(wheel_key) {
            Some(id) if mode_def(id).is_some() => id,
            _ => self.cfg.wheel_list(wheel_key).iter().find(|id| mode_def(id).is_some()).map_or("volume", |id| id),
        }
    }

    pub fn is_learning(&self) -> bool {
        self.learn.is_some()
    }

    /// Feed one media key from the hook. Decides whether it is the knob
    /// (learned keys, optional device filter), then runs it like [`Engine::handle`].
    pub fn handle_input(&mut self, ev: &InputEvent, now: Instant, front_app: Option<&str>) -> (Decision, Vec<Effect>) {
        let mut fx = Vec::new();
        if self.learn.is_some() {
            self.learn_input(ev, now, &mut fx);
            return (Decision::Swallow, fx);
        }
        let Some(mut key) = self.cfg.knob_keys.knob_key(ev.key) else { return (Decision::Pass, fx) };
        if self.cfg.invert_direction {
            key = match key {
                KnobKey::Clockwise => KnobKey::CounterClockwise,
                KnobKey::CounterClockwise => KnobKey::Clockwise,
                KnobKey::Press => KnobKey::Press,
            };
        }
        if self.cfg.only_knob_device {
            if let Some(knob) = &self.cfg.knob_device {
                if !ev.device.as_ref().is_some_and(|d| d.same_device(knob)) {
                    return (Decision::Pass, fx);
                }
            }
        }
        if self.preview {
            if !ev.repeat {
                fx.push(Effect::Preview { key, down: ev.down });
            }
            return (Decision::Swallow, fx);
        }
        let knob = KnobEvent { key, down: ev.down, repeat: ev.repeat, modifier: ev.modifier };
        let (decision, more) = self.handle(knob, now, front_app);
        fx.extend(more);
        // "Pass" means "let the OS change the volume". That only works if the
        // knob sends the matching volume key; otherwise change it ourselves.
        if decision == Decision::Pass && key != KnobKey::Press {
            let wanted = if key == KnobKey::Clockwise { MediaKey::VolumeUp } else { MediaKey::VolumeDown };
            if ev.key != wanted {
                if ev.down {
                    fx.push(Effect::Perform(Action::Media(wanted)));
                }
                return (Decision::Swallow, fx);
            }
        }
        (decision, fx)
    }

    /// Start "learn my knob": the next three distinct media keys become
    /// clockwise, counter-clockwise and press. Every media key is swallowed
    /// until it finishes, is cancelled, or times out.
    pub fn start_learn(&mut self, now: Instant) -> Vec<Effect> {
        let mut fx = Vec::new();
        self.close_wheel(&mut fx);
        self.release_held(&mut fx);
        self.deadlines.tap = None;
        self.deadlines.hold = None;
        self.learn = Some(Learn { step: KnobKey::Clockwise, keys: Vec::new(), device: None });
        self.deadlines.learn = Some(now + ms(LEARN_TIMEOUT_MS));
        fx.push(Effect::Learn(LearnEvent::Prompt { step: KnobKey::Clockwise }));
        fx
    }

    pub fn cancel_learn(&mut self) -> Vec<Effect> {
        let mut fx = Vec::new();
        self.deadlines.learn = None;
        if self.learn.take().is_some() {
            fx.push(Effect::Learn(LearnEvent::Cancelled));
        }
        fx
    }

    /// While on, knob keys are swallowed and reported as [`Effect::Preview`].
    pub fn set_preview(&mut self, on: bool) -> Vec<Effect> {
        let mut fx = Vec::new();
        if on && !self.preview {
            self.close_wheel(&mut fx);
            self.release_held(&mut fx);
            self.deadlines.tap = None;
            self.deadlines.hold = None;
        }
        self.preview = on;
        fx
    }

    /// Feed one knob key. Returns immediately what to do with the original
    /// event, plus effects to run afterwards.
    pub fn handle(&mut self, ev: KnobEvent, now: Instant, front_app: Option<&str>) -> (Decision, Vec<Effect>) {
        let mut fx = Vec::new();
        let key = self.wheel_key_for(front_app);
        let decision = match ev.key {
            KnobKey::Press => {
                if !ev.repeat {
                    if ev.down {
                        self.press_down(now, &key, &mut fx);
                    } else {
                        self.press_up(now, &key, &mut fx);
                    }
                }
                Decision::Swallow
            }
            KnobKey::Clockwise | KnobKey::CounterClockwise => {
                let pass = if ev.down {
                    let dir = if ev.key == KnobKey::Clockwise { 1 } else { -1 };
                    let pass = self.rotate(dir, ev.modifier, now, &key, &mut fx);
                    self.passing.insert(ev.key, pass);
                    pass
                } else {
                    match self.passing.remove(&ev.key) {
                        Some(pass) => pass,
                        // key-up without a key-down we saw: treat like a fresh turn would be
                        None => self.wheel.is_none() && self.current_mode(&key) == "volume",
                    }
                };
                if pass {
                    Decision::Pass
                } else {
                    Decision::Swallow
                }
            }
        };
        (decision, fx)
    }

    /// Run every timer that is due.
    pub fn tick(&mut self, now: Instant) -> Vec<Effect> {
        let mut fx = Vec::new();
        if take_due(&mut self.deadlines.tap, now) {
            let key = self.press_wheel_key.clone();
            self.run_press(&key, &mut fx);
        }
        if take_due(&mut self.deadlines.hold, now) {
            let key = self.press_wheel_key.clone();
            self.open_wheel(now, &key, OpenVia::Hold, &mut fx);
        }
        if take_due(&mut self.deadlines.confirm, now) {
            self.commit(now, false, &mut fx);
        }
        if take_due(&mut self.deadlines.wheel_timeout, now) {
            self.close_wheel(&mut fx);
        }
        if take_due(&mut self.deadlines.app_switch, now) {
            self.end_app_switch(&mut fx);
        }
        if take_due(&mut self.deadlines.drag, now) {
            self.end_drag(&mut fx);
        }
        if take_due(&mut self.deadlines.revert, now) {
            let key = self.revert_wheel_key.clone();
            self.set_mode_inner(&key, "volume", &mut fx);
        }
        if take_due(&mut self.deadlines.learn, now) && self.learn.take().is_some() {
            fx.push(Effect::Learn(LearnEvent::Cancelled));
        }
        fx
    }

    /// When [`Engine::tick`] should run next.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.deadlines.all().into_iter().flatten().min()
    }

    /// The wheel modifier was released. A wheel it opened picks the
    /// highlight, or just closes if the highlight never moved.
    pub fn modifier_released(&mut self, now: Instant) -> Vec<Effect> {
        let mut fx = Vec::new();
        if let Some(w) = &self.wheel {
            if w.via == OpenVia::Modifier {
                if w.sel != w.start_sel {
                    self.commit(now, true, &mut fx);
                } else {
                    self.close_wheel(&mut fx);
                }
            }
        }
        fx
    }

    /// Close the wheel without picking (Esc).
    pub fn cancel_wheel(&mut self) -> Vec<Effect> {
        let mut fx = Vec::new();
        self.close_wheel(&mut fx);
        fx
    }

    /// Pick a mode directly (tray menu). Unknown ids are ignored.
    pub fn set_mode(&mut self, wheel_key: &str, mode: &str) -> Vec<Effect> {
        let mut fx = Vec::new();
        if mode_def(mode).is_some() {
            self.close_wheel(&mut fx);
            self.set_mode_inner(wheel_key, mode, &mut fx);
        }
        fx
    }

    /// The frontmost app changed: let go of held modifiers / mouse buttons.
    pub fn app_changed(&mut self) -> Vec<Effect> {
        let mut fx = Vec::new();
        self.release_held(&mut fx);
        fx
    }

    // --- knob press -------------------------------------------------------

    fn press_down(&mut self, now: Instant, key: &str, fx: &mut Vec<Effect>) {
        if self.wheel.as_ref().is_some_and(|w| w.sticky) {
            self.commit(now, false, fx);
            self.swallow_next_up = true;
            return;
        }
        match self.cfg.trigger {
            Trigger::DoubleTap => {
                if self.deadlines.tap.take().is_some() {
                    // second tap: open the wheel instead of running the single-tap action
                    self.open_wheel(now, key, OpenVia::Tap, fx);
                    self.swallow_next_up = true;
                }
            }
            Trigger::Hold => {
                self.pressing = true;
                self.press_wheel_key = key.to_string();
                self.deadlines.hold = Some(now + ms(self.cfg.hold_ms));
            }
        }
    }

    fn press_up(&mut self, now: Instant, key: &str, fx: &mut Vec<Effect>) {
        self.pressing = false;
        if std::mem::take(&mut self.swallow_next_up) {
            return;
        }
        match self.cfg.trigger {
            Trigger::DoubleTap => {
                self.press_wheel_key = key.to_string();
                self.deadlines.tap = Some(now + ms(self.cfg.double_tap_ms));
            }
            Trigger::Hold => {
                if self.deadlines.hold.take().is_some() {
                    let key = self.press_wheel_key.clone();
                    self.run_press(&key, fx);
                    return;
                }
                let Some(w) = &mut self.wheel else { return };
                if w.rotated_while_held {
                    self.commit(now, false, fx);
                } else {
                    w.sticky = true;
                    self.deadlines.wheel_timeout = Some(now + ms(self.cfg.wheel_timeout_ms));
                }
            }
        }
    }

    /// The current mode's tap action.
    fn run_press(&mut self, key: &str, fx: &mut Vec<Effect>) {
        let mode = self.current_mode(key).to_string();
        let action = match mode.as_str() {
            "volume" => Action::Media(MediaKey::Mute),
            "zoom" => Action::ZoomReset,
            "spaces" => Action::DesktopOverview,
            "media" | "seek" => Action::Media(MediaKey::PlayPause),
            "jog" | "edits" => Action::SpaceKey,
            "apps" => return self.end_app_switch(fx),
            "scroll" => {
                self.scroll_axis = self.scroll_axis.toggled();
                return hud(fx, "scroll", format!("Scroll: {}", self.scroll_axis.label()));
            }
            "drag" => {
                self.end_drag(fx);
                self.drag_axis = self.drag_axis.toggled();
                return hud(fx, "drag", format!("Drag: {}", self.drag_axis.label()));
            }
            _ => return,
        };
        fx.push(Effect::Perform(action));
    }

    // --- knob rotation ----------------------------------------------------

    /// Returns true if the original key should pass through.
    fn rotate(&mut self, dir: i8, modifier: bool, now: Instant, key: &str, fx: &mut Vec<Effect>) -> bool {
        if let Some(w) = &mut self.wheel {
            if self.pressing {
                w.rotated_while_held = true;
            }
            w.sel = (w.sel as isize + dir as isize).rem_euclid(w.list.len() as isize) as usize;
            fx.push(Effect::Ui(UiEvent::Select { selected: w.sel }));
            self.deadlines.wheel_timeout = Some(now + ms(self.cfg.wheel_timeout_ms));
            // One-off actions (typing text, opening apps) are never fired by just
            // pausing on them; only a tap or the modifier release runs them.
            let via = w.via;
            let one_off = matches!(self.cfg.slice_def(&w.list[w.sel]), Some(SliceDef::Action(a)) if !matches!(a.kind, ActionKind::Wheel { .. }));
            self.deadlines.confirm =
                (self.auto_confirms(via) && !one_off).then(|| now + ms(self.cfg.auto_confirm_ms));
            return false;
        }

        // Modifier + turn opens the wheel on the current mode; later turns move it.
        // Not while Apps mode holds its own (synthetic) Cmd / Alt down.
        if modifier && self.cfg.modifier_trigger && !self.app_switch_active {
            self.open_wheel(now, key, OpenVia::Modifier, fx);
            return false;
        }

        let mode = self.current_mode(key).to_string();
        self.arm_revert(now, key, &mode);
        let d = dir as i32;
        let action = match mode.as_str() {
            "brightness" => Action::Media(if dir > 0 { MediaKey::BrightnessUp } else { MediaKey::BrightnessDown }),
            "scroll" => {
                let factor = self.scroll_acceleration(now, dir);
                if self.cfg.smooth_scroll {
                    let pixels = (self.cfg.scroll_pixels as f32 * factor).round() as i32;
                    Action::SmoothScroll { axis: self.scroll_axis, pixels: d * pixels }
                } else {
                    let lines = (self.cfg.scroll_lines as f32 * factor).round() as i32;
                    Action::Scroll { axis: self.scroll_axis, lines: d * lines }
                }
            }
            "zoom" => {
                if dir > 0 {
                    Action::ZoomIn
                } else {
                    Action::ZoomOut
                }
            }
            "apps" => {
                self.app_switch_active = true;
                self.deadlines.app_switch = Some(now + ms(self.cfg.app_switch_commit_ms));
                Action::AppSwitchStep(dir)
            }
            "windows" => Action::WindowCycle(dir),
            "tabs" => Action::TabCycle(dir),
            "spaces" => Action::DesktopStep(dir),
            "media" => Action::Media(if dir > 0 { MediaKey::Next } else { MediaKey::Previous }),
            "seek" => Action::Seek { seconds: d * self.cfg.seek_seconds },
            "drag" => {
                self.drag_active = true;
                self.deadlines.drag = Some(now + ms(self.cfg.drag_release_ms));
                Action::DragStep { axis: self.drag_axis, pixels: d * self.cfg.drag_pixels }
            }
            "jog" => Action::FrameStep(dir),
            "edits" => Action::EditPoint(dir),
            "undo" => Action::UndoRedo(dir),
            // "volume" and anything unknown: the OS changes the volume itself
            _ => return true,
        };
        fx.push(Effect::Perform(action));
        false
    }

    /// 1.0 for slow turns, up to `scroll_acceleration` for fast ones in the
    /// same direction.
    fn scroll_acceleration(&mut self, now: Instant, dir: i8) -> f32 {
        let max = self.cfg.scroll_acceleration;
        let factor = match self.last_scroll {
            Some((t, d)) if d == dir && max > 1.0 => {
                let dt = now.saturating_duration_since(t).as_secs_f32() * 1000.0;
                let fast = ((ACCEL_SLOW_MS - dt) / (ACCEL_SLOW_MS - ACCEL_FAST_MS)).clamp(0.0, 1.0);
                1.0 + (max - 1.0) * fast
            }
            _ => 1.0,
        };
        self.last_scroll = Some((now, dir));
        factor
    }

    fn arm_revert(&mut self, now: Instant, key: &str, mode: &str) {
        self.deadlines.revert = None;
        if self.cfg.revert_after_ms > 0 && mode != "volume" {
            self.revert_wheel_key = key.to_string();
            self.deadlines.revert = Some(now + ms(self.cfg.revert_after_ms));
        }
    }

    // --- wheel ------------------------------------------------------------

    /// Modifier wheels pick on modifier release instead.
    fn auto_confirms(&self, via: OpenVia) -> bool {
        self.cfg.auto_confirm_ms > 0 && via != OpenVia::Modifier
    }

    fn open_wheel(&mut self, now: Instant, key: &str, via: OpenVia, fx: &mut Vec<Effect>) {
        self.release_held(fx);
        let list = self.cfg.wheel_list(key).to_vec();
        let active = list.iter().position(|id| id == self.current_mode(key));
        self.show_wheel(now, key, key, false, list, active, via, fx);
    }

    /// Replace the open wheel with a sub-wheel. Modes picked there are saved
    /// under the parent's key.
    fn open_sub_wheel(&mut self, now: Instant, parent_key: &str, sub: &CustomAction, via: OpenVia, fx: &mut Vec<Effect>) {
        let ActionKind::Wheel { slices } = &sub.kind else { return };
        let list = slices.clone();
        let active = list.iter().position(|id| id == self.current_mode(parent_key));
        let title = sub.name.clone();
        self.show_wheel(now, parent_key, &title, true, list, active, via, fx);
    }

    #[allow(clippy::too_many_arguments)]
    fn show_wheel(
        &mut self,
        now: Instant,
        key: &str,
        title: &str,
        nested: bool,
        list: Vec<String>,
        active: Option<usize>,
        via: OpenVia,
        fx: &mut Vec<Effect>,
    ) {
        let selected = active.unwrap_or(0);
        let slices = list.iter().filter_map(|id| self.slice(id)).collect();
        fx.push(Effect::Ui(UiEvent::Open {
            wheel: title.to_string(),
            nested,
            slices,
            selected,
            active,
            via,
            auto_confirm_ms: if self.auto_confirms(via) { self.cfg.auto_confirm_ms } else { 0 },
        }));
        self.wheel = Some(OpenWheel {
            key: key.to_string(),
            list,
            sel: selected,
            start_sel: selected,
            via,
            sticky: via != OpenVia::Hold,
            rotated_while_held: false,
        });
        self.deadlines.confirm = None;
        self.deadlines.wheel_timeout = Some(now + ms(self.cfg.wheel_timeout_ms));
    }

    fn slice(&self, id: &str) -> Option<Slice> {
        Some(match self.cfg.slice_def(id)? {
            SliceDef::Mode(m) => Slice {
                id: m.id.into(),
                name: m.name.into(),
                hint: m.hint.into(),
                kind: SliceKind::Mode,
                icon: None,
                color: None,
            },
            SliceDef::Action(a) => Slice {
                id: a.id.clone(),
                name: a.name.clone(),
                hint: action_hint(&a.kind),
                kind: if matches!(a.kind, ActionKind::Wheel { .. }) { SliceKind::Wheel } else { SliceKind::Action },
                icon: a.icon.clone(),
                color: a.color.clone(),
            },
        })
    }

    fn close_wheel(&mut self, fx: &mut Vec<Effect>) {
        self.deadlines.confirm = None;
        self.deadlines.wheel_timeout = None;
        if self.wheel.take().is_some() {
            fx.push(Effect::Ui(UiEvent::Close));
        }
    }

    /// Pick the highlighted slice. `modifier_released`: the modifier that
    /// opened this wheel is no longer held, so a sub-wheel must not wait for it.
    fn commit(&mut self, now: Instant, modifier_released: bool, fx: &mut Vec<Effect>) {
        let Some(w) = self.wheel.take() else { return };
        self.deadlines.confirm = None;
        self.deadlines.wheel_timeout = None;
        let id = &w.list[w.sel];
        let action = match self.cfg.slice_def(id) {
            Some(SliceDef::Mode(_)) | None => None,
            Some(SliceDef::Action(a)) => Some(a.clone()),
        };
        match action {
            None => {
                fx.push(Effect::Ui(UiEvent::Close));
                self.set_mode_inner(&w.key, id, fx);
            }
            Some(a) if matches!(a.kind, ActionKind::Wheel { .. }) => {
                let via = if w.via == OpenVia::Modifier && !modifier_released { OpenVia::Modifier } else { OpenVia::Tap };
                self.open_sub_wheel(now, &w.key, &a, via, fx);
            }
            Some(a) => {
                fx.push(Effect::Ui(UiEvent::Close));
                if let Some(action) = custom_action(&a.kind) {
                    fx.push(Effect::Perform(action));
                }
                fx.push(Effect::Ui(UiEvent::Hud { mode: a.id.clone(), text: a.name.clone(), icon: a.icon.clone(), color: a.color.clone() }));
            }
        }
    }

    // --- learn ----------------------------------------------------------------

    fn learn_input(&mut self, ev: &InputEvent, now: Instant, fx: &mut Vec<Effect>) {
        let Some(learn) = &mut self.learn else { return };
        if !ev.down || ev.repeat || learn.keys.contains(&ev.key) {
            return;
        }
        learn.keys.push(ev.key);
        if learn.device.is_none() {
            learn.device.clone_from(&ev.device);
        }
        self.deadlines.learn = Some(now + ms(LEARN_TIMEOUT_MS));
        let next = match learn.step {
            KnobKey::Clockwise => Some(KnobKey::CounterClockwise),
            KnobKey::CounterClockwise => Some(KnobKey::Press),
            KnobKey::Press => None,
        };
        match next {
            Some(step) => {
                learn.step = step;
                fx.push(Effect::Learn(LearnEvent::Prompt { step }));
            }
            None => {
                let keys = KnobKeys { clockwise: learn.keys[0], counter_clockwise: learn.keys[1], press: learn.keys[2] };
                let device = learn.device.take();
                self.learn = None;
                self.deadlines.learn = None;
                fx.push(Effect::Learn(LearnEvent::Done { keys, device }));
            }
        }
    }

    fn set_mode_inner(&mut self, key: &str, mode: &str, fx: &mut Vec<Effect>) {
        self.release_held(fx);
        self.deadlines.revert = None;
        self.modes.insert(key.to_string(), mode.to_string());
        fx.push(Effect::ModeChanged { wheel: key.to_string(), mode: mode.to_string() });
        let name = mode_def(mode).map_or(mode, |m| m.name);
        hud(fx, mode, name.to_string());
    }

    // --- held state ---------------------------------------------------------

    fn release_held(&mut self, fx: &mut Vec<Effect>) {
        self.end_app_switch(fx);
        self.end_drag(fx);
    }

    fn end_app_switch(&mut self, fx: &mut Vec<Effect>) {
        self.deadlines.app_switch = None;
        if std::mem::take(&mut self.app_switch_active) {
            fx.push(Effect::Perform(Action::AppSwitchEnd));
        }
    }

    fn end_drag(&mut self, fx: &mut Vec<Effect>) {
        self.deadlines.drag = None;
        if std::mem::take(&mut self.drag_active) {
            fx.push(Effect::Perform(Action::DragEnd));
        }
    }
}

fn hud(fx: &mut Vec<Effect>, mode: &str, text: String) {
    fx.push(Effect::Ui(UiEvent::Hud { mode: mode.to_string(), text, icon: None, color: None }));
}

fn custom_action(kind: &ActionKind) -> Option<Action> {
    Some(match kind {
        // validated by Config::sanitized, so parsing only fails on a bug
        ActionKind::Keys { keys } => Action::Shortcut(KeyCombo::parse_sequence(keys).ok()?),
        ActionKind::Open { target } => Action::Open(target.clone()),
        ActionKind::Text { text } => Action::TypeText(text.clone()),
        ActionKind::Media { key } => Action::Media(*key),
        ActionKind::Wheel { .. } => return None,
    })
}

fn action_hint(kind: &ActionKind) -> String {
    match kind {
        ActionKind::Keys { keys } => format!("tap: press\n{keys}"),
        ActionKind::Open { target } => format!("tap: open\n{target}"),
        ActionKind::Text { .. } => "tap: type text".into(),
        ActionKind::Media { key } => format!("tap: {}", media_label(*key)),
        ActionKind::Wheel { slices } => format!("opens {} more", slices.len()),
    }
}

fn media_label(key: MediaKey) -> &'static str {
    match key {
        MediaKey::VolumeUp => "volume up",
        MediaKey::VolumeDown => "volume down",
        MediaKey::Mute => "mute",
        MediaKey::PlayPause => "play / pause",
        MediaKey::Next => "next track",
        MediaKey::Previous => "previous track",
        MediaKey::BrightnessUp => "brightness up",
        MediaKey::BrightnessDown => "brightness down",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_WHEEL_KEY;
    use KnobKey::*;

    const RESOLVE: Option<&str> = Some("DaVinci Resolve");

    struct Rig {
        e: Engine,
        now: Instant,
        app: Option<&'static str>,
    }

    impl Rig {
        fn new() -> Self {
            Self::with(Config::default())
        }

        fn with(cfg: Config) -> Self {
            Self { e: Engine::new(cfg, HashMap::new()), now: Instant::now(), app: None }
        }

        fn ev(&mut self, key: KnobKey, down: bool) -> (Decision, Vec<Effect>) {
            let ev = if down { KnobEvent::down(key) } else { KnobEvent::up(key) };
            self.e.handle(ev, self.now, self.app)
        }

        /// Down + up; returns both decisions and all effects.
        fn click(&mut self, key: KnobKey) -> ([Decision; 2], Vec<Effect>) {
            let (d1, mut fx) = self.ev(key, true);
            let (d2, fx2) = self.ev(key, false);
            fx.extend(fx2);
            ([d1, d2], fx)
        }

        /// Move the clock forward, running timers the way the app's timer thread would.
        fn wait(&mut self, millis: u64) -> Vec<Effect> {
            let end = self.now + ms(millis);
            let mut fx = Vec::new();
            while let Some(t) = self.e.next_deadline().filter(|t| *t <= end) {
                self.now = t;
                fx.extend(self.e.tick(t));
            }
            self.now = end;
            fx
        }

        fn mode(&self) -> &str {
            let key = self.e.wheel_key_for(self.app);
            self.e.current_mode(&key)
        }

        fn open_wheel(&mut self) -> Vec<Effect> {
            let (_, mut fx) = self.click(Press);
            fx.extend(self.wait(100));
            fx.extend(self.click(Press).1);
            fx
        }
    }

    fn performed(fx: &[Effect]) -> Vec<Action> {
        fx.iter().filter_map(|f| if let Effect::Perform(a) = f { Some(a.clone()) } else { None }).collect()
    }

    fn has_ui(fx: &[Effect], pred: impl Fn(&UiEvent) -> bool) -> bool {
        fx.iter().any(|f| matches!(f, Effect::Ui(u) if pred(u)))
    }

    #[test]
    fn single_tap_mutes_after_the_double_tap_window() {
        let mut r = Rig::new();
        let (d, fx) = r.click(Press);
        assert_eq!(d, [Decision::Swallow; 2]);
        assert!(fx.is_empty());
        assert!(r.wait(279).is_empty());
        assert_eq!(performed(&r.wait(1)), vec![Action::Media(MediaKey::Mute)]);
    }

    #[test]
    fn double_tap_opens_wheel_and_never_mutes() {
        let mut r = Rig::new();
        let fx = r.open_wheel();
        assert!(has_ui(&fx, |u| matches!(u, UiEvent::Open { selected: 0, active: Some(0), .. })));
        assert!(r.e.is_wheel_open());
        assert!(performed(&r.wait(10_000)).is_empty());
    }

    #[test]
    fn volume_mode_turns_pass_through_down_and_up() {
        let mut r = Rig::new();
        let (d, fx) = r.click(Clockwise);
        assert_eq!(d, [Decision::Pass; 2]);
        assert!(fx.is_empty());
        assert_eq!(r.click(CounterClockwise).0, [Decision::Pass; 2]);
    }

    #[test]
    fn turning_in_wheel_is_swallowed_and_auto_confirms_after_600ms() {
        let mut r = Rig::new();
        r.open_wheel();
        let (d, fx) = r.click(Clockwise);
        assert_eq!(d, [Decision::Swallow; 2]);
        assert!(has_ui(&fx, |u| *u == UiEvent::Select { selected: 1 }));
        assert!(r.wait(599).is_empty());
        let fx = r.wait(1);
        assert!(has_ui(&fx, |u| *u == UiEvent::Close));
        assert!(fx.contains(&Effect::ModeChanged { wheel: DEFAULT_WHEEL_KEY.into(), mode: "brightness".into() }));
        assert!(has_ui(&fx, |u| matches!(u, UiEvent::Hud { mode, .. } if mode == "brightness")));
        assert_eq!(r.mode(), "brightness");
        assert!(!r.e.is_wheel_open());
    }

    #[test]
    fn each_turn_restarts_the_confirm_countdown() {
        let mut r = Rig::new();
        r.open_wheel();
        r.click(Clockwise);
        r.wait(500);
        r.click(Clockwise);
        r.wait(500);
        assert!(r.e.is_wheel_open());
        r.wait(100);
        assert_eq!(r.mode(), "scroll");
    }

    #[test]
    fn wrap_around_counter_clockwise() {
        let mut r = Rig::new();
        r.open_wheel();
        let fx = r.click(CounterClockwise).1;
        assert!(has_ui(&fx, |u| *u == UiEvent::Select { selected: 8 }));
        r.wait(1000);
        assert_eq!(r.mode(), "seek");
    }

    #[test]
    fn tap_in_wheel_picks_now_and_its_release_runs_nothing() {
        let mut r = Rig::new();
        r.open_wheel();
        r.click(Clockwise);
        let (_, fx) = r.click(Press);
        assert_eq!(r.mode(), "brightness");
        assert!(has_ui(&fx, |u| *u == UiEvent::Close));
        assert!(performed(&r.wait(5000)).is_empty());
    }

    #[test]
    fn untouched_wheel_closes_after_timeout_without_changing_mode() {
        let mut r = Rig::new();
        r.open_wheel();
        assert!(r.wait(4999).is_empty());
        let fx = r.wait(1);
        assert_eq!(fx, vec![Effect::Ui(UiEvent::Close)]);
        assert_eq!(r.mode(), "volume");
    }

    #[test]
    fn cancel_closes_without_picking() {
        let mut r = Rig::new();
        r.open_wheel();
        r.click(Clockwise);
        assert_eq!(r.e.cancel_wheel(), vec![Effect::Ui(UiEvent::Close)]);
        assert!(r.wait(5000).is_empty());
        assert_eq!(r.mode(), "volume");
    }

    #[test]
    fn scroll_mode_scrolls_and_swallows_both_halves() {
        let mut r = Rig::new();
        r.e.set_mode(DEFAULT_WHEEL_KEY, "scroll");
        let (d, fx) = r.click(Clockwise);
        assert_eq!(d, [Decision::Swallow; 2]);
        assert_eq!(performed(&fx), vec![Action::SmoothScroll { axis: Axis::Vertical, pixels: 40 }]);
        r.click(Press);
        r.wait(300);
        let fx = r.click(CounterClockwise).1;
        assert_eq!(performed(&fx), vec![Action::SmoothScroll { axis: Axis::Horizontal, pixels: -40 }]);
    }

    #[test]
    fn line_scroll_when_smooth_scroll_is_off() {
        let mut r = Rig::with(Config { smooth_scroll: false, ..Config::default() });
        r.e.set_mode(DEFAULT_WHEEL_KEY, "scroll");
        assert_eq!(performed(&r.click(Clockwise).1), vec![Action::Scroll { axis: Axis::Vertical, lines: 3 }]);
    }

    #[test]
    fn fast_scrolling_accelerates_up_to_the_cap_and_resets_on_reversal() {
        let mut r = Rig::new();
        r.e.set_mode(DEFAULT_WHEEL_KEY, "scroll");
        let px = |fx: &[Effect]| match performed(fx).as_slice() {
            [Action::SmoothScroll { pixels, .. }] => *pixels,
            other => panic!("{other:?}"),
        };
        assert_eq!(px(&r.click(Clockwise).1), 40);
        r.wait(115); // halfway between slow (200ms) and fast (30ms)
        assert_eq!(px(&r.click(Clockwise).1), 100);
        r.wait(10);
        assert_eq!(px(&r.click(Clockwise).1), 160);
        r.wait(10);
        assert_eq!(px(&r.click(Clockwise).1), 160);
        r.wait(10);
        assert_eq!(px(&r.click(CounterClockwise).1), -40);
        r.wait(500);
        assert_eq!(px(&r.click(CounterClockwise).1), -40);
    }

    #[test]
    fn key_up_mirrors_its_key_down_even_if_mode_changed_between() {
        let mut r = Rig::new();
        assert_eq!(r.ev(Clockwise, true).0, Decision::Pass);
        r.e.set_mode(DEFAULT_WHEEL_KEY, "scroll");
        assert_eq!(r.ev(Clockwise, false).0, Decision::Pass);
        assert_eq!(r.ev(Clockwise, true).0, Decision::Swallow);
        r.e.set_mode(DEFAULT_WHEEL_KEY, "volume");
        assert_eq!(r.ev(Clockwise, false).0, Decision::Swallow);
    }

    #[test]
    fn stray_key_up_follows_current_mode() {
        let mut r = Rig::new();
        assert_eq!(r.ev(Clockwise, false).0, Decision::Pass);
        r.e.set_mode(DEFAULT_WHEEL_KEY, "zoom");
        assert_eq!(r.ev(Clockwise, false).0, Decision::Swallow);
    }

    #[test]
    fn app_switcher_is_released_after_idle_or_when_the_wheel_opens() {
        let mut r = Rig::new();
        r.e.set_mode(DEFAULT_WHEEL_KEY, "apps");
        assert_eq!(performed(&r.click(Clockwise).1), vec![Action::AppSwitchStep(1)]);
        r.wait(500);
        assert_eq!(performed(&r.click(Clockwise).1), vec![Action::AppSwitchStep(1)]);
        assert!(r.wait(799).is_empty());
        assert_eq!(performed(&r.wait(1)), vec![Action::AppSwitchEnd]);

        r.click(CounterClockwise);
        let fx = r.open_wheel();
        assert_eq!(performed(&fx), vec![Action::AppSwitchEnd]);
    }

    #[test]
    fn apps_tap_releases_switcher_once() {
        let mut r = Rig::new();
        r.e.set_mode(DEFAULT_WHEEL_KEY, "apps");
        r.click(Clockwise);
        r.click(Press);
        assert_eq!(performed(&r.wait(300)), vec![Action::AppSwitchEnd]);
        assert!(performed(&r.wait(2000)).is_empty());
    }

    #[test]
    fn drag_releases_after_idle_and_tap_switches_axis() {
        let mut r = Rig::new();
        r.e.set_mode(DEFAULT_WHEEL_KEY, "drag");
        assert_eq!(
            performed(&r.click(Clockwise).1),
            vec![Action::DragStep { axis: Axis::Horizontal, pixels: 4 }]
        );
        assert_eq!(performed(&r.wait(350)), vec![Action::DragEnd]);
        r.click(Clockwise);
        r.click(Press);
        let fx = r.wait(300);
        assert_eq!(performed(&fx), vec![Action::DragEnd]);
        assert!(has_ui(&fx, |u| matches!(u, UiEvent::Hud { text, .. } if text == "Drag: vertical")));
    }

    #[test]
    fn per_app_wheel_has_its_own_slices_and_mode() {
        let mut r = Rig::new();
        r.app = RESOLVE;
        let fx = r.open_wheel();
        assert!(has_ui(&fx, |u| matches!(u, UiEvent::Open { wheel, slices, .. }
            if wheel == "DaVinci Resolve" && slices[1].id == "jog")));
        r.click(Clockwise);
        r.wait(1000);
        assert_eq!(r.mode(), "jog");
        assert_eq!(performed(&r.click(Clockwise).1), vec![Action::FrameStep(1)]);

        r.app = Some("Safari");
        assert_eq!(r.mode(), "volume");
        assert_eq!(r.click(Clockwise).0, [Decision::Pass; 2]);
    }

    #[test]
    fn tap_action_uses_the_app_that_was_frontmost_when_tapped() {
        let mut r = Rig::new();
        r.app = RESOLVE;
        r.e.set_mode("DaVinci Resolve", "jog");
        r.click(Press);
        r.app = None;
        assert_eq!(performed(&r.wait(300)), vec![Action::SpaceKey]);
    }

    #[test]
    fn press_repeats_are_swallowed_and_ignored() {
        let mut r = Rig::new();
        let ev = KnobEvent { repeat: true, ..KnobEvent::down(Press) };
        let (d, fx) = r.e.handle(ev, r.now, None);
        assert_eq!(d, Decision::Swallow);
        assert!(fx.is_empty());
        assert!(r.wait(1000).is_empty());
    }

    #[test]
    fn tray_mode_not_on_wheel_still_counts() {
        let mut r = Rig::new();
        r.e.set_mode(DEFAULT_WHEEL_KEY, "undo");
        assert_eq!(performed(&r.click(CounterClockwise).1), vec![Action::UndoRedo(-1)]);
        let fx = r.open_wheel();
        assert!(has_ui(&fx, |u| matches!(u, UiEvent::Open { selected: 0, active: None, .. })));
    }

    #[test]
    fn revert_to_volume_after_idle() {
        let mut r = Rig::with(Config { revert_after_ms: 3000, ..Config::default() });
        r.e.set_mode(DEFAULT_WHEEL_KEY, "scroll");
        r.click(Clockwise);
        r.wait(2999);
        assert_eq!(r.mode(), "scroll");
        r.wait(1);
        assert_eq!(r.mode(), "volume");
    }

    #[test]
    fn auto_confirm_off_means_only_tap_picks() {
        let mut r = Rig::with(Config { auto_confirm_ms: 0, ..Config::default() });
        r.open_wheel();
        r.click(Clockwise);
        r.wait(3000);
        assert!(r.e.is_wheel_open());
        r.click(Press);
        assert_eq!(r.mode(), "brightness");
    }

    mod hold_trigger {
        use super::*;

        fn rig() -> Rig {
            Rig::with(Config { trigger: Trigger::Hold, ..Config::default() })
        }

        #[test]
        fn short_tap_runs_press_action_on_release() {
            let mut r = rig();
            assert_eq!(performed(&r.click(Press).1), vec![Action::Media(MediaKey::Mute)]);
        }

        #[test]
        fn hold_opens_then_turn_and_release_picks() {
            let mut r = rig();
            r.ev(Press, true);
            assert!(r.wait(349).is_empty());
            assert!(has_ui(&r.wait(1), |u| matches!(u, UiEvent::Open { .. })));
            r.click(Clockwise);
            r.click(Clockwise);
            let (_, fx) = r.ev(Press, false);
            assert!(has_ui(&fx, |u| *u == UiEvent::Close));
            assert_eq!(r.mode(), "scroll");
        }

        #[test]
        fn hold_release_without_turning_leaves_wheel_open_for_a_tap() {
            let mut r = rig();
            r.ev(Press, true);
            r.wait(400);
            r.ev(Press, false);
            assert!(r.e.is_wheel_open());
            r.click(Clockwise);
            r.click(Press);
            assert_eq!(r.mode(), "brightness");
            assert!(performed(&r.wait(1000)).is_empty());
        }
    }

    mod modifier_trigger {
        use super::*;

        pub(super) fn turn(r: &mut Rig, key: KnobKey) -> ([Decision; 2], Vec<Effect>) {
            let (d1, mut fx) = r.e.handle(KnobEvent::down(key).with_modifier(true), r.now, r.app);
            let (d2, fx2) = r.e.handle(KnobEvent::up(key).with_modifier(true), r.now, r.app);
            fx.extend(fx2);
            ([d1, d2], fx)
        }

        #[test]
        fn first_turn_opens_on_current_mode_without_moving() {
            let mut r = Rig::new();
            let (d, fx) = turn(&mut r, Clockwise);
            assert_eq!(d, [Decision::Swallow; 2]);
            assert!(has_ui(&fx, |u| matches!(u, UiEvent::Open { selected: 0, via: OpenVia::Modifier, auto_confirm_ms: 0, .. })));
            assert!(!has_ui(&fx, |u| matches!(u, UiEvent::Select { .. })));
        }

        #[test]
        fn release_picks_and_there_is_no_auto_confirm_while_held() {
            let mut r = Rig::new();
            turn(&mut r, Clockwise);
            turn(&mut r, Clockwise);
            turn(&mut r, Clockwise);
            r.wait(3000);
            assert!(r.e.is_wheel_open());
            let fx = r.e.modifier_released(r.now);
            assert!(has_ui(&fx, |u| *u == UiEvent::Close));
            assert_eq!(r.mode(), "scroll");
        }

        #[test]
        fn release_without_moving_just_closes() {
            let mut r = Rig::new();
            turn(&mut r, CounterClockwise);
            let fx = r.e.modifier_released(r.now);
            assert_eq!(fx, vec![Effect::Ui(UiEvent::Close)]);
            assert_eq!(r.mode(), "volume");
        }

        #[test]
        fn release_does_not_touch_a_double_tap_wheel() {
            let mut r = Rig::new();
            r.open_wheel();
            r.click(Clockwise);
            assert!(r.e.modifier_released(r.now).is_empty());
            assert!(r.e.is_wheel_open());
        }

        #[test]
        fn tap_still_picks_a_modifier_wheel() {
            let mut r = Rig::new();
            turn(&mut r, Clockwise);
            turn(&mut r, Clockwise);
            r.click(Press);
            assert_eq!(r.mode(), "brightness");
            assert!(r.e.modifier_released(r.now).is_empty());
        }

        #[test]
        fn disabled_in_config_means_modifier_turns_act_normally() {
            let mut r = Rig::with(Config { modifier_trigger: false, ..Config::default() });
            assert_eq!(turn(&mut r, Clockwise).0, [Decision::Pass; 2]);
            assert!(!r.e.is_wheel_open());
        }

        #[test]
        fn apps_mode_own_cmd_does_not_open_the_wheel() {
            let mut r = Rig::new();
            r.e.set_mode(DEFAULT_WHEEL_KEY, "apps");
            r.click(Clockwise);
            let (_, fx) = turn(&mut r, Clockwise);
            assert_eq!(performed(&fx), vec![Action::AppSwitchStep(1)]);
            assert!(!r.e.is_wheel_open());
        }
    }


    // --- custom actions and sub-wheels ------------------------------------------

    fn with_actions() -> Config {
        Config::from_json(
            r#"{
              "wheel": ["volume", "reopen", "dev", "scroll"],
              "actions": [
                { "id": "reopen", "name": "Reopen tab", "icon": "rotate-ccw", "type": "keys", "keys": "cmd+shift+t" },
                { "id": "term", "name": "Terminal", "type": "open", "target": "Ghostty" },
                { "id": "dev", "name": "Dev", "type": "wheel", "slices": ["term", "zoom", "undo"] }
              ]
            }"#,
        )
        .unwrap()
    }

    fn open_slices(fx: &[Effect]) -> Option<(String, bool, Vec<Slice>, OpenVia)> {
        fx.iter().find_map(|f| match f {
            Effect::Ui(UiEvent::Open { wheel, nested, slices, via, .. }) => Some((wheel.clone(), *nested, slices.clone(), *via)),
            _ => None,
        })
    }

    #[test]
    fn action_slices_carry_kind_icon_and_hint() {
        let mut r = Rig::with(with_actions());
        let (_, nested, slices, _) = open_slices(&r.open_wheel()).unwrap();
        assert!(!nested);
        assert_eq!(slices[0].kind, SliceKind::Mode);
        assert_eq!(slices[1].kind, SliceKind::Action);
        assert_eq!(slices[1].icon.as_deref(), Some("rotate-ccw"));
        assert_eq!(slices[1].hint, "tap: press\ncmd+shift+t");
        assert_eq!(slices[2].kind, SliceKind::Wheel);
    }

    #[test]
    fn pausing_on_an_action_never_runs_it() {
        let mut r = Rig::with(with_actions());
        r.open_wheel();
        r.click(Clockwise); // "reopen"
        assert!(performed(&r.wait(10_000)).is_empty());
        assert_eq!(r.mode(), "volume");
    }

    #[test]
    fn tap_runs_an_action_once_and_keeps_the_mode() {
        let mut r = Rig::with(with_actions());
        r.e.set_mode(DEFAULT_WHEEL_KEY, "scroll");
        r.open_wheel();
        r.click(CounterClockwise); // from scroll (3) back to dev (2)
        r.click(CounterClockwise); // reopen (1)
        let (_, fx) = r.click(Press);
        let combo = KeyCombo::parse("cmd+shift+t").unwrap();
        assert_eq!(performed(&fx), vec![Action::Shortcut(vec![combo])]);
        assert!(has_ui(&fx, |u| matches!(u, UiEvent::Hud { mode, icon, .. } if mode == "reopen" && icon.as_deref() == Some("rotate-ccw"))));
        assert!(!fx.iter().any(|f| matches!(f, Effect::ModeChanged { .. })));
        assert_eq!(r.mode(), "scroll");
        assert!(!r.e.is_wheel_open());
        assert!(performed(&r.wait(1000)).is_empty());
    }

    #[test]
    fn auto_confirm_on_a_sub_wheel_opens_it_and_modes_save_under_the_parent() {
        let mut r = Rig::with(with_actions());
        r.app = None;
        r.open_wheel();
        r.click(Clockwise);
        r.click(Clockwise); // "dev"
        let fx = r.wait(600);
        let (title, nested, slices, via) = open_slices(&fx).unwrap();
        assert_eq!((title.as_str(), nested, via), ("Dev", true, OpenVia::Tap));
        assert_eq!(slices.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["term", "zoom", "undo"]);
        assert!(r.e.is_wheel_open());

        r.click(Clockwise); // zoom
        let fx = r.wait(600);
        assert!(fx.contains(&Effect::ModeChanged { wheel: DEFAULT_WHEEL_KEY.into(), mode: "zoom".into() }));
        assert_eq!(r.mode(), "zoom");
    }

    #[test]
    fn tap_on_open_action_in_sub_wheel() {
        let mut r = Rig::with(with_actions());
        r.open_wheel();
        r.click(Clockwise);
        r.click(Clockwise);
        r.click(Press); // open "dev" now
        let (_, fx) = r.click(Press); // "term" is selected first
        assert_eq!(performed(&fx), vec![Action::Open("Ghostty".into())]);
        assert!(!r.e.is_wheel_open());
    }

    #[test]
    fn modifier_wheel_release_runs_the_action() {
        let mut r = Rig::with(with_actions());
        modifier_trigger::turn(&mut r, Clockwise); // opens on volume
        modifier_trigger::turn(&mut r, Clockwise); // reopen
        assert!(performed(&r.wait(3000)).is_empty());
        let fx = r.e.modifier_released(r.now);
        assert_eq!(performed(&fx).len(), 1);
    }

    #[test]
    fn modifier_sub_wheel_keeps_waiting_for_release_when_tapped_open() {
        let mut r = Rig::with(with_actions());
        modifier_trigger::turn(&mut r, Clockwise);
        modifier_trigger::turn(&mut r, Clockwise);
        modifier_trigger::turn(&mut r, Clockwise); // dev
        let (_, fx) = r.click(Press);
        assert_eq!(open_slices(&fx).unwrap().3, OpenVia::Modifier);
        modifier_trigger::turn(&mut r, Clockwise); // zoom
        r.e.modifier_released(r.now);
        assert_eq!(r.mode(), "zoom");
    }

    #[test]
    fn modifier_release_on_a_sub_wheel_slice_opens_it_as_tap() {
        let mut r = Rig::with(with_actions());
        modifier_trigger::turn(&mut r, Clockwise);
        modifier_trigger::turn(&mut r, Clockwise);
        modifier_trigger::turn(&mut r, Clockwise); // dev
        let fx = r.e.modifier_released(r.now);
        assert_eq!(open_slices(&fx).unwrap().3, OpenVia::Tap);
        assert!(r.e.is_wheel_open());
    }

    #[test]
    fn first_mode_is_the_fallback_even_if_an_action_comes_first() {
        let mut cfg = with_actions();
        cfg.wheel = vec!["reopen".into(), "scroll".into()];
        let r = Rig::with(cfg);
        assert_eq!(r.mode(), "scroll");
    }

    // --- raw input: learned keys, device filter, learn, preview ---------------

    fn input(key: MediaKey, down: bool) -> InputEvent {
        InputEvent::new(key, down)
    }

    fn feed(r: &mut Rig, ev: InputEvent) -> (Decision, Vec<Effect>) {
        r.e.handle_input(&ev, r.now, r.app)
    }

    fn press_input(r: &mut Rig, key: MediaKey) -> (Decision, Vec<Effect>) {
        let (d, mut fx) = feed(r, input(key, true));
        fx.extend(feed(r, input(key, false)).1);
        (d, fx)
    }

    const F75: DeviceInfo = DeviceInfo { vendor_id: 13652, product_id: 64007, name: String::new() };

    #[test]
    fn default_knob_keys_are_the_volume_keys() {
        let mut r = Rig::new();
        assert_eq!(feed(&mut r, input(MediaKey::VolumeUp, true)).0, Decision::Pass);
        assert_eq!(feed(&mut r, input(MediaKey::PlayPause, true)).0, Decision::Pass);
        assert_eq!(feed(&mut r, input(MediaKey::Mute, true)).0, Decision::Swallow);
    }

    #[test]
    fn learned_non_volume_keys_still_change_the_volume_in_volume_mode() {
        let knob_keys = KnobKeys { clockwise: MediaKey::Next, counter_clockwise: MediaKey::Previous, press: MediaKey::PlayPause };
        let mut r = Rig::with(Config { knob_keys, ..Config::default() });
        let (d, fx) = feed(&mut r, input(MediaKey::Next, true));
        assert_eq!(d, Decision::Swallow);
        assert_eq!(performed(&fx), vec![Action::Media(MediaKey::VolumeUp)]);
        let (d, fx) = feed(&mut r, input(MediaKey::Next, false));
        assert_eq!(d, Decision::Swallow);
        assert!(fx.is_empty());
        assert_eq!(performed(&feed(&mut r, input(MediaKey::Previous, true)).1), vec![Action::Media(MediaKey::VolumeDown)]);
        // the real volume keys are no longer the knob
        assert_eq!(feed(&mut r, input(MediaKey::VolumeUp, true)).0, Decision::Pass);
        // press = play/pause key: tap still mutes in volume mode
        press_input(&mut r, MediaKey::PlayPause);
        assert_eq!(performed(&r.wait(300)), vec![Action::Media(MediaKey::Mute)]);
    }

    #[test]
    fn device_filter_lets_other_keyboards_through() {
        let mut r = Rig::with(Config { knob_device: Some(F75), only_knob_device: true, ..Config::default() });
        r.e.set_mode(DEFAULT_WHEEL_KEY, "scroll");
        let laptop = DeviceInfo { vendor_id: 1452, product_id: 1, name: "Apple".into() };
        assert_eq!(feed(&mut r, InputEvent { device: Some(laptop), ..input(MediaKey::VolumeUp, true) }).0, Decision::Pass);
        assert_eq!(feed(&mut r, input(MediaKey::VolumeUp, true)).0, Decision::Pass, "unknown device is not the knob");
        let (d, fx) = feed(&mut r, InputEvent { device: Some(F75), ..input(MediaKey::VolumeUp, true) });
        assert_eq!(d, Decision::Swallow);
        assert_eq!(performed(&fx).len(), 1);
    }

    #[test]
    fn invert_direction_flips_turns_including_volume() {
        let mut r = Rig::with(Config { invert_direction: true, ..Config::default() });
        let (d, fx) = feed(&mut r, input(MediaKey::VolumeUp, true));
        assert_eq!(d, Decision::Swallow);
        assert_eq!(performed(&fx), vec![Action::Media(MediaKey::VolumeDown)]);
        r.e.set_mode(DEFAULT_WHEEL_KEY, "tabs");
        assert_eq!(performed(&feed(&mut r, input(MediaKey::VolumeDown, true)).1), vec![Action::TabCycle(1)]);
        r.e.set_preview(true);
        assert_eq!(feed(&mut r, input(MediaKey::VolumeUp, true)).1, vec![Effect::Preview { key: CounterClockwise, down: true }]);
    }

    #[test]
    fn device_filter_off_ignores_the_device() {
        let mut r = Rig::with(Config { knob_device: Some(F75), ..Config::default() });
        r.e.set_mode(DEFAULT_WHEEL_KEY, "zoom");
        assert_eq!(feed(&mut r, input(MediaKey::VolumeUp, true)).0, Decision::Swallow);
    }

    fn learned(fx: &[Effect]) -> Vec<LearnEvent> {
        fx.iter().filter_map(|f| if let Effect::Learn(l) = f { Some(l.clone()) } else { None }).collect()
    }

    #[test]
    fn learn_takes_three_distinct_keys_and_swallows_everything_meanwhile() {
        let mut r = Rig::new();
        r.open_wheel();
        let fx = r.e.start_learn(r.now);
        assert!(has_ui(&fx, |u| *u == UiEvent::Close));
        assert_eq!(learned(&fx), vec![LearnEvent::Prompt { step: Clockwise }]);

        let (d, fx) = feed(&mut r, InputEvent { device: Some(F75), ..input(MediaKey::Next, true) });
        assert_eq!(d, Decision::Swallow);
        assert_eq!(learned(&fx), vec![LearnEvent::Prompt { step: CounterClockwise }]);
        assert_eq!(feed(&mut r, input(MediaKey::Next, false)).0, Decision::Swallow);
        // more clockwise detents are ignored
        assert!(press_input(&mut r, MediaKey::Next).1.is_empty());
        assert_eq!(learned(&press_input(&mut r, MediaKey::Previous).1), vec![LearnEvent::Prompt { step: Press }]);
        let fx = press_input(&mut r, MediaKey::PlayPause).1;
        let keys = KnobKeys { clockwise: MediaKey::Next, counter_clockwise: MediaKey::Previous, press: MediaKey::PlayPause };
        assert_eq!(learned(&fx), vec![LearnEvent::Done { keys, device: Some(F75) }]);
        assert!(!r.e.is_learning());
        assert!(r.wait(60_000).is_empty());
        assert_eq!(feed(&mut r, input(MediaKey::VolumeUp, true)).0, Decision::Pass);
    }

    #[test]
    fn learn_times_out_and_can_be_cancelled() {
        let mut r = Rig::new();
        r.e.start_learn(r.now);
        r.wait(10_000);
        press_input(&mut r, MediaKey::VolumeUp); // progress restarts the timeout
        assert!(learned(&r.wait(19_999)).is_empty());
        assert_eq!(learned(&r.wait(1)), vec![LearnEvent::Cancelled]);
        assert!(!r.e.is_learning());
        assert_eq!(feed(&mut r, input(MediaKey::VolumeUp, true)).0, Decision::Pass);

        r.e.start_learn(r.now);
        assert_eq!(learned(&r.e.cancel_learn()), vec![LearnEvent::Cancelled]);
        assert!(r.e.cancel_learn().is_empty());
        assert!(r.wait(30_000).is_empty());
    }

    #[test]
    fn preview_swallows_knob_keys_and_reports_them() {
        let mut r = Rig::new();
        r.open_wheel();
        let fx = r.e.set_preview(true);
        assert!(has_ui(&fx, |u| *u == UiEvent::Close));
        let (d, fx) = feed(&mut r, input(MediaKey::VolumeUp, true));
        assert_eq!(d, Decision::Swallow);
        assert_eq!(fx, vec![Effect::Preview { key: Clockwise, down: true }]);
        let (d, fx) = press_input(&mut r, MediaKey::Mute);
        assert_eq!(d, Decision::Swallow);
        assert_eq!(fx.len(), 2);
        assert!(r.wait(1000).is_empty(), "no mute after the double-tap window");
        assert_eq!(feed(&mut r, input(MediaKey::PlayPause, true)).0, Decision::Pass);

        r.e.set_preview(false);
        assert_eq!(feed(&mut r, input(MediaKey::VolumeUp, true)).0, Decision::Pass);
    }

    #[test]
    fn ui_events_serialize_with_type_tag() {
        let json = serde_json::to_value(UiEvent::Select { selected: 3 }).unwrap();
        assert_eq!(json, serde_json::json!({ "type": "select", "selected": 3 }));
        let json = serde_json::to_value(UiEvent::Close).unwrap();
        assert_eq!(json, serde_json::json!({ "type": "close" }));
    }
}
