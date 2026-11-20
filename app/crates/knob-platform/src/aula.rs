//! Lights on an AULA F75 keyboard, through its 2.4G receiver (Windows).
//!
//! The receiver's vendor collection (usage page 0xFF02) takes 20-byte output
//! reports: `[0x13, command, packet count, index, (sub << 4) | length,
//! 14 payload bytes, checksum]`. Command 0x88 shows a "real-time" frame: the
//! keyboard keeps it until the next one and goes back to its own effect about
//! 2 s after the last. Nothing is stored and nothing is acknowledged.
//! Protocol notes: github.com/bakiakyol/aula-f75-hub (docs/protocol.md); both
//! frame kinds used here were checked on a real F75 over its receiver.
//!
//! Frames go out from a thread of their own: packets need about 5 ms between
//! them, and after a volume key the volume is read a few times while Windows
//! applies it.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::windows::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};

use crate::KeyboardLights;

const VENDOR_ID: u32 = 0x3554;
/// The 2.4G receiver. Wired (258A:010C) and Bluetooth use other transports.
const RECEIVER_ID: u32 = 0xFA09;
const USAGE_PAGE: u16 = 0xFF02;
const REPORT_ID: u8 = 0x13;
const REAL_TIME: u8 = 0x88;
/// Real-time frame: groups of `[R, G, B, count, index...]`; keys not listed are dark.
const SUB_KEYS: u8 = 1;
/// Real-time frame: `[R, G, B]` for the whole board.
const SUB_BOARD: u8 = 2;
const PAYLOAD: usize = 14;
/// Lighting index of F1; F2 to F12 follow every 6 (Dev/kb/1/KB.ini of the AULA app).
const F1: u8 = 12;
/// Closer packets get lost; 5 ms plays smoothly.
const PACKET_GAP: Duration = Duration::from_millis(5);
/// About as many frames a second as the receiver carries.
const FRAME_GAP: Duration = Duration::from_millis(70);
/// The last frame goes out once more, in case the radio lost it.
const RESEND_AFTER: Duration = Duration::from_millis(150);
/// After a volume key, read the volume this long, this often.
const VOLUME_WATCH: Duration = Duration::from_millis(300);
const VOLUME_POLL: Duration = Duration::from_millis(25);
/// No receiver: look again at most this often.
const LOOKUP_EVERY: Duration = Duration::from_secs(3);
/// Brightness of the unlit part of the level bar, and of the lit part while muted.
const UNLIT: f32 = 0.08;
const MUTED: f32 = 0.25;
/// Brightness of the glow: a hint of colour, not a flash.
const GLOW: f32 = 0.35;
/// The keyboard drops a frame after about 2 s; a held glow is re-sent this often.
const GLOW_REFRESH: Duration = Duration::from_secs(1);
/// A glow nobody updated or ended for this long ends anyway (say the runtime
/// stopped with the wheel open).
const GLOW_MAX: Duration = Duration::from_secs(10);

/// Colours while on; None while off.
static COLORS: Mutex<Option<KeyboardLights>> = Mutex::new(None);
/// Same as `COLORS.is_some()`, without a lock, for the hook thread.
static ON: AtomicBool = AtomicBool::new(false);
/// The lights thread's inbox. Started the first time the lights are turned on.
static INBOX: LazyLock<Option<mpsc::Sender<Msg>>> = LazyLock::new(|| {
    let (tx, rx) = mpsc::channel();
    match thread::Builder::new().name("knob-lights".into()).spawn(move || run(&rx)) {
        Ok(_) => Some(tx),
        Err(e) => {
            log::warn!("could not start the keyboard lights thread: {e}");
            None
        }
    }
});

enum Msg {
    Glow(Option<[u8; 3]>),
    VolumeKey,
    Brightness(u32),
}

pub fn set_keyboard_lights(lights: Option<KeyboardLights>) {
    let on = lights.is_some();
    *COLORS.lock().unwrap_or_else(PoisonError::into_inner) = lights;
    if on {
        LazyLock::force(&INBOX);
    }
    ON.store(on, SeqCst);
}

/// Holds a soft glow of `color` on the whole board until the next call; None
/// ends it (the keyboard keeps it about 2 s, then its own effect returns).
pub fn glow_keyboard(color: Option<[u8; 3]>) {
    send(Msg::Glow(color));
}

/// A volume or mute key is on its way to Windows. Called from the hook, so it
/// only queues.
pub(crate) fn volume_key() {
    send(Msg::VolumeKey);
}

pub(crate) fn brightness_changed(percent: u32) {
    send(Msg::Brightness(percent));
}

fn send(msg: Msg) {
    if ON.load(SeqCst) {
        if let Some(tx) = &*INBOX {
            let _ = tx.send(msg);
        }
    }
}

fn run(rx: &mpsc::Receiver<Msg>) {
    // SAFETY: once, on this thread, which lives as long as the process.
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok();
    if let Err(e) = &com {
        log::warn!("keyboard lights: CoInitializeEx failed, no volume bar: {e}");
    }
    let mut out = Receiver::default();
    let mut audio: Option<IMMDeviceEnumerator> = None;
    let mut watch: Option<VolumeWatch> = None;
    // The glow being held: its frame, next refresh, and when it gives up.
    let mut glow: Option<(Frame, Instant, Instant)> = None;
    // The newest frame not sent yet, and the last one sent (to repeat once).
    let mut pending: Option<Frame> = None;
    let mut resend: Option<(Instant, Frame)> = None;
    loop {
        let wake = [
            pending.as_ref().map(|_| out.next_send()),
            watch.as_ref().map(|w| w.next_poll),
            resend.as_ref().map(|(at, _)| *at),
            glow.as_ref().map(|(_, refresh, _)| *refresh),
        ]
        .into_iter()
        .flatten()
        .min();
        let msg = match wake {
            Some(at) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(msg) => Some(msg),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            },
            None => match rx.recv() {
                Ok(msg) => Some(msg),
                Err(_) => return,
            },
        };
        let Some(colors) = *COLORS.lock().unwrap_or_else(PoisonError::into_inner) else {
            // turned off: drop whatever was under way
            (pending, resend, watch, glow) = (None, None, None, None);
            continue;
        };
        let now = Instant::now();
        match msg {
            Some(Msg::Glow(Some(color))) => {
                let frame = Frame::board(color, GLOW);
                pending = Some(frame.clone());
                glow = Some((frame, now + GLOW_REFRESH, now + GLOW_MAX));
            }
            // no frame: the keyboard keeps the last glow ~2 s, then its own effect
            Some(Msg::Glow(None)) => glow = None,
            Some(Msg::Brightness(percent)) => pending = Some(Frame::level(colors.brightness, percent as f32 / 100.0, false)),
            Some(Msg::VolumeKey) => match &mut watch {
                Some(w) => w.until = now + VOLUME_WATCH,
                None if com.is_ok() => match VolumeWatch::start(&mut audio, now) {
                    Ok(w) => watch = Some(w),
                    Err(e) => log::debug!("keyboard lights: no volume control: {e}"),
                },
                None => {}
            },
            None => {}
        }
        if let Some(w) = watch.as_mut().filter(|w| now >= w.next_poll) {
            match w.poll(now) {
                Ok(Some((level, muted))) => pending = Some(Frame::level(colors.volume, level, muted)),
                Ok(None) => {}
                Err(e) => {
                    log::debug!("keyboard lights: reading the volume failed: {e}");
                    w.until = now;
                }
            }
        }
        if watch.as_ref().is_some_and(|w| now >= w.until) {
            watch = None;
        }
        if glow.as_ref().is_some_and(|(_, _, until)| now >= *until) {
            glow = None;
        } else if let Some((frame, refresh, _)) = glow.as_mut().filter(|(_, refresh, _)| now >= *refresh) {
            *refresh = now + GLOW_REFRESH;
            pending.get_or_insert_with(|| frame.clone());
        }
        if let Some(frame) = pending.take_if(|_| out.ready(now)) {
            out.send(&frame);
            resend = Some((Instant::now() + RESEND_AFTER, frame));
        } else if pending.is_none() && resend.as_ref().is_some_and(|(at, _)| now >= *at) {
            if let Some((_, frame)) = resend.take() {
                out.send(&frame);
            }
        }
    }
}

/// Reads the volume for a moment after a volume key, while Windows applies it.
struct VolumeWatch {
    control: IAudioEndpointVolume,
    until: Instant,
    next_poll: Instant,
    /// Percent and mute state last shown.
    shown: Option<(u32, bool)>,
}

impl VolumeWatch {
    /// Uses the default output device as of now (it can change between keys).
    fn start(audio: &mut Option<IMMDeviceEnumerator>, now: Instant) -> windows::core::Result<Self> {
        let devices = match audio {
            Some(devices) => devices,
            // SAFETY: COM is initialized on this thread (MTA).
            None => audio.insert(unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }?),
        };
        // SAFETY: as above; no activation parameters.
        let control = unsafe { devices.GetDefaultAudioEndpoint(eRender, eConsole)?.Activate(CLSCTX_ALL, None) }?;
        Ok(Self { control, until: now + VOLUME_WATCH, next_poll: now, shown: None })
    }

    /// The level (0 to 1) and mute state, if they changed since the last poll.
    /// The first poll always reports, so a key at 0 % or 100 % still shows.
    fn poll(&mut self, now: Instant) -> windows::core::Result<Option<(f32, bool)>> {
        self.next_poll = now + VOLUME_POLL;
        // SAFETY: plain getters on a live endpoint.
        let (level, muted) = unsafe { (self.control.GetMasterVolumeLevelScalar()?, self.control.GetMute()?.as_bool()) };
        let seen = ((level * 100.0).round() as u32, muted);
        Ok((self.shown.replace(seen) != Some(seen)).then_some((level, muted)))
    }
}

/// The receiver's vendor channel: opened on first use and again after errors.
#[derive(Default)]
struct Receiver {
    file: Option<File>,
    looked: Option<Instant>,
    sent: Option<Instant>,
}

impl Receiver {
    fn ready(&self, now: Instant) -> bool {
        self.sent.is_none_or(|at| now >= at + FRAME_GAP)
    }

    fn next_send(&self) -> Instant {
        self.sent.map_or_else(Instant::now, |at| at + FRAME_GAP)
    }

    fn send(&mut self, frame: &Frame) {
        self.sent = Some(Instant::now());
        let Some(file) = self.open() else { return };
        let written = frame.packets().enumerate().try_for_each(|(i, packet)| {
            if i > 0 {
                thread::sleep(PACKET_GAP);
            }
            file.write_all(&packet)
        });
        if let Err(e) = written {
            log::debug!("keyboard lights: write failed: {e}");
            self.file = None;
        }
    }

    fn open(&mut self) -> Option<&mut File> {
        if self.file.is_none() && self.looked.is_none_or(|at| at.elapsed() >= LOOKUP_EVERY) {
            self.looked = Some(Instant::now());
            self.file = receiver_path().and_then(|path| {
                OpenOptions::new()
                    .write(true)
                    .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
                    .open(&path)
                    .inspect(|_| log::info!("keyboard lights: AULA F75 receiver at {path}"))
                    .inspect_err(|e| log::debug!("keyboard lights: cannot open {path}: {e}"))
                    .ok()
            });
        }
        self.file.as_mut()
    }
}

/// Device path of the receiver's vendor collection, if it is plugged in.
fn receiver_path() -> Option<String> {
    let found = crate::windows::hid_devices().into_iter().find(|(_, hid)| {
        hid.dwVendorId == VENDOR_ID && hid.dwProductId == RECEIVER_ID && hid.usUsagePage == USAGE_PAGE
    });
    if found.is_none() {
        log::debug!("keyboard lights: no AULA F75 receiver");
    }
    crate::windows::device_name(found?.0)
}

/// A real-time frame: sub-command and data, split over as many packets as needed.
#[derive(Clone, Debug, PartialEq)]
struct Frame {
    sub: u8,
    data: Vec<u8>,
}

impl Frame {
    /// The whole board in `color`, at `brightness` (0 to 1).
    fn board(color: [u8; 3], brightness: f32) -> Self {
        Self { sub: SUB_BOARD, data: led(color).map(|c| (f32::from(c) * brightness).round() as u8).to_vec() }
    }

    /// F1 to F12 as a bar: `level` (0 to 1) of it lit from the left, the rest
    /// faint, everything else dark. Every key is its own group, so no key
    /// changes group from one frame to the next (that flickers).
    fn level(color: [u8; 3], level: f32, muted: bool) -> Self {
        let color = led(color);
        let full = if muted { MUTED } else { 1.0 };
        let mut data = Vec::with_capacity(12 * 5);
        for i in 0..12u8 {
            let lit = (level * 12.0 - f32::from(i)).clamp(0.0, 1.0);
            let k = UNLIT + (full - UNLIT) * lit;
            data.extend(color.map(|c| (f32::from(c) * k).round() as u8));
            data.extend([1, F1 + 6 * i]);
        }
        Self { sub: SUB_KEYS, data }
    }

    fn packets(&self) -> impl Iterator<Item = [u8; 20]> + '_ {
        let count = self.data.len().div_ceil(PAYLOAD) as u8;
        self.data.chunks(PAYLOAD).enumerate().map(move |(i, chunk)| packet(REAL_TIME, count, i as u8, self.sub, chunk))
    }
}

/// One output report.
fn packet(command: u8, count: u8, index: u8, sub: u8, payload: &[u8]) -> [u8; 20] {
    let mut p = [0u8; 20];
    p[..4].copy_from_slice(&[REPORT_ID, command, count, index]);
    p[4] = (sub << 4) | payload.len() as u8;
    p[5..5 + payload.len()].copy_from_slice(payload);
    p[19] = p[..19].iter().fold(0, |sum: u8, &b| sum.wrapping_add(b));
    p
}

/// Screen colours, pastels especially, come out nearly white on RGB LEDs:
/// keep the hue, at full saturation and brightness. Greys light up white.
fn led([r, g, b]: [u8; 3]) -> [u8; 3] {
    let (hi, lo) = (r.max(g).max(b), r.min(g).min(b));
    if hi - lo < 16 {
        return [255; 3];
    }
    let span = f32::from(hi - lo);
    [r, g, b].map(|c| (f32::from(c - lo) / span * 255.0).round() as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_framing_matches_the_keyboard() {
        // The LED block read request the F75 answered (checksum 0x58).
        let mut want = [0u8; 20];
        want[..3].copy_from_slice(&[0x13, 0x44, 0x01]);
        want[19] = 0x58;
        assert_eq!(packet(0x44, 1, 0, 0, &[]), want);
    }

    #[test]
    fn level_bar_lights_keys_from_f1() {
        let frame = Frame::level([0, 255, 0], 0.5, false);
        let keys: Vec<(u8, u8)> = frame.data.chunks(5).map(|g| (g[4], g[1])).collect();
        let indices: Vec<u8> = keys.iter().map(|k| k.0).collect();
        assert_eq!(indices, (0..12).map(|i| 12 + 6 * i).collect::<Vec<u8>>());
        // F1-F6 lit, F7-F12 faint
        assert!(keys[..6].iter().all(|k| k.1 == 255));
        assert!(keys[6..].iter().all(|k| k.1 == 20));

        let packets: Vec<[u8; 20]> = frame.packets().collect();
        assert_eq!(packets.len(), 5);
        assert!(packets.iter().enumerate().all(|(i, p)| p[1] == 0x88 && p[2] == 5 && p[3] == i as u8));
        assert_eq!(packets[4][4], (1 << 4) | 4, "60 bytes: four full packets and 4 left");
    }

    #[test]
    fn led_saturates_pastels_and_whitens_greys() {
        // the "scroll" pastel
        assert_eq!(led([0xad, 0xf5, 0xdd]), [0, 255, 170]);
        assert_eq!(led([128, 128, 128]), [255; 3]);
    }
}
