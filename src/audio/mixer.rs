//! Several audio sources mixed into the one track a clip has.
//!
//! [`MixSource`] decides what to capture from the user's Sound choices and
//! keeps that up to date while recording (an app starts or quits, a device
//! is plugged in, a box is ticked):
//! * **Whole PC audio**: a loopback capture of every playback device, so
//!   Discord on the headset's "Chat" device is recorded along with the game
//!   on its "Game" device.
//! * **Only the ticked apps**: a per-app capture of each ticked app that has
//!   audio open (see `process.rs`). On Windows versions without per-app
//!   capture, the devices those apps play on are captured instead.
//! * **Microphone** (your voice), in either mode, if switched on.
//!
//! [`MixBuffer`] sums the timestamped packets of all sources on the
//! recording's timeline (used by the recorder's audio thread).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::devices;
use super::loopback::LoopbackSource;
use super::process::ProcessLoopbackSource;
use super::{AudioPacket, AudioSource, CHANNELS};
use crate::config::AudioSettings;

/// The Sound choices, shared between the controller (which updates them
/// when settings change) and the recorder's audio thread (which applies
/// them without restarting the recording).
pub struct SharedSelection {
    inner: Mutex<AudioSettings>,
    version: AtomicU64,
}

impl SharedSelection {
    pub fn new(s: AudioSettings) -> Arc<SharedSelection> {
        Arc::new(SharedSelection { inner: Mutex::new(s), version: AtomicU64::new(1) })
    }

    pub fn set(&self, s: AudioSettings) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if *g != s {
            *g = s;
            self.version.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn get(&self) -> (u64, AudioSettings) {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        (self.version.load(Ordering::Relaxed), g.clone())
    }
}

/// What is being captured right now, for the window's Sound tab.
#[derive(Clone, Debug, Default)]
pub struct CaptureState {
    pub sources: Vec<String>,
    pub note: Option<String>,
}

pub static CAPTURE_STATE: Mutex<CaptureState> = Mutex::new(CaptureState { sources: Vec::new(), note: None });

pub fn capture_state() -> CaptureState {
    CAPTURE_STATE.lock().map(|g| g.clone()).unwrap_or_default()
}

struct Child {
    key: String,
    label: String,
    source: Box<dyn AudioSource>,
    stream: u32,
    gain: f32,
}

pub struct MixSource {
    selection: Arc<SharedSelection>,
    seen_version: u64,
    children: Vec<Child>,
    next_stream: u32,
    last_refresh: Option<Instant>,
    /// Per-app capture works on this PC (`None` = not tried yet).
    per_app_ok: Option<bool>,
    scratch: Vec<AudioPacket>,
}

// SAFETY: created and used on the recorder's audio thread only.
unsafe impl Send for MixSource {}

impl MixSource {
    pub fn new(selection: Arc<SharedSelection>) -> MixSource {
        MixSource {
            selection,
            seen_version: 0,
            children: Vec::new(),
            next_stream: 1,
            last_refresh: None,
            per_app_ok: None,
            scratch: Vec::new(),
        }
    }

    /// What should be captured now: (key, label, gain) per source.
    fn wanted(&mut self, s: &AudioSettings) -> (Vec<(String, String, f32)>, Option<String>) {
        let (mut out, note) = self.wanted_playback(s);
        if s.record_mic {
            let mics = devices::capture_devices().unwrap_or_default();
            let mic = if s.mic_device.is_empty() {
                mics.iter().find(|m| m.is_default)
            } else {
                mics.iter().find(|m| m.id == s.mic_device)
            };
            let name = mic.map(|m| m.name.clone()).unwrap_or_else(|| "Microphone".into());
            // "mic:" alone = follow the default microphone.
            out.push((format!("mic:{}", s.mic_device), name, s.mic_volume));
        }
        (out, note)
    }

    fn wanted_playback(&mut self, s: &AudioSettings) -> (Vec<(String, String, f32)>, Option<String>) {
        let mut out = Vec::new();
        let devices = devices::render_devices().unwrap_or_default();
        if s.whole_pc {
            for d in &devices {
                let choice = s.devices.iter().find(|c| c.id == d.id);
                if choice.is_some_and(|c| !c.include) {
                    continue;
                }
                out.push((format!("dev:{}", d.id), d.name.clone(), choice.map(|c| c.volume).unwrap_or(1.0)));
            }
            return (out, None);
        }
        let ticked: HashMap<String, f32> =
            s.apps.iter().filter(|a| a.include).map(|a| (a.exe.to_ascii_lowercase(), a.volume)).collect();
        if ticked.is_empty() {
            return (out, Some("No apps are ticked, so no app sound is recorded.".into()));
        }
        // Sessions of ticked apps (playing or just open).
        let mut pids: Vec<(u32, String, f32, String)> = Vec::new(); // pid, exe, gain, device
        for sess in devices::sessions() {
            let Some(path) = crate::win::process_exe_path(sess.pid) else { continue };
            let key = devices::exe_key(&path);
            if let Some(&gain) = ticked.get(&key) {
                if !pids.iter().any(|p| p.0 == sess.pid) {
                    pids.push((sess.pid, devices::app_display_name(&path), gain, sess.device_id.clone()));
                }
            }
        }
        if self.per_app_ok != Some(false) {
            // A process inside another captured process's tree is already
            // included in that capture.
            let parents = devices::parent_pids();
            let roots: Vec<u32> = pids.iter().map(|p| p.0).collect();
            for (pid, name, gain, _) in &pids {
                if roots.iter().any(|r| r != pid && devices::is_descendant(*pid, *r, &parents)) {
                    continue;
                }
                out.push((format!("pid:{pid}"), name.clone(), *gain));
            }
            return (out, None);
        }
        // Fallback: whole devices the ticked apps play on.
        let mut seen = Vec::new();
        for (_, _, _, dev) in &pids {
            if !seen.contains(dev) {
                seen.push(dev.clone());
                let name = devices.iter().find(|d| &d.id == dev).map(|d| d.name.clone()).unwrap_or_default();
                out.push((format!("dev:{dev}"), name, 1.0));
            }
        }
        let note = "Per-app capture needs Windows 10 version 2004 or newer, so Snag records the whole \
                    devices your ticked apps play on (other sounds on them are included).";
        (out, Some(note.into()))
    }

    fn refresh(&mut self) {
        let (version, s) = self.selection.get();
        self.seen_version = version;
        self.last_refresh = Some(Instant::now());
        let (mut wanted, mut note) = self.wanted(&s);

        // Drop sources no longer wanted (or whose app quit).
        self.children.retain(|c| wanted.iter().any(|w| w.0 == c.key));
        for (key, label, gain) in wanted.clone() {
            if let Some(c) = self.children.iter_mut().find(|c| c.key == key) {
                c.gain = gain;
                c.label = label;
                continue;
            }
            let source: Option<Box<dyn AudioSource>> = if let Some(id) = key.strip_prefix("dev:") {
                Some(Box::new(LoopbackSource::for_device(id)))
            } else if let Some(id) = key.strip_prefix("mic:") {
                Some(Box::new(LoopbackSource::microphone(Some(id).filter(|i| !i.is_empty()))))
            } else if let Some(pid) = key.strip_prefix("pid:").and_then(|p| p.parse().ok()) {
                match ProcessLoopbackSource::new(pid) {
                    Ok(s) => {
                        self.per_app_ok = Some(true);
                        log::info!("PC audio: capturing {label} (process {pid})");
                        Some(Box::new(s))
                    }
                    Err(e) => {
                        log::warn!("PC audio: per-app capture of {label} failed: {e:#}");
                        if self.per_app_ok.is_none() {
                            // Never worked on this PC: switch to the device fallback.
                            self.per_app_ok = Some(false);
                            self.children.clear();
                            let (w, n) = self.wanted(&s);
                            wanted = w;
                            note = n;
                            return self.apply_fallback(wanted, note);
                        }
                        None
                    }
                }
            } else {
                None
            };
            if let Some(source) = source {
                self.children.push(Child { key, label, source, stream: self.next_stream, gain });
                self.next_stream += 1;
            }
        }
        self.publish(note);
    }

    fn apply_fallback(&mut self, wanted: Vec<(String, String, f32)>, note: Option<String>) {
        for (key, label, gain) in wanted {
            let source = if let Some(id) = key.strip_prefix("dev:") {
                LoopbackSource::for_device(id)
            } else if let Some(id) = key.strip_prefix("mic:") {
                LoopbackSource::microphone(Some(id).filter(|i| !i.is_empty()))
            } else {
                continue;
            };
            {
                let source = Box::new(source);
                self.children.push(Child { key, label, source, stream: self.next_stream, gain });
                self.next_stream += 1;
            }
        }
        self.publish(note);
    }

    fn publish(&self, note: Option<String>) {
        let sources = self.children.iter().map(|c| c.label.clone()).collect::<Vec<_>>();
        if let Ok(mut g) = CAPTURE_STATE.lock() {
            if g.sources != sources {
                log::info!("PC audio sources: {}", if sources.is_empty() { "none".into() } else { sources.join(", ") });
            }
            *g = CaptureState { sources, note };
        }
    }
}

impl AudioSource for MixSource {
    fn poll(&mut self, out: &mut Vec<AudioPacket>, start: u64, now: u64) {
        let changed = self.selection.version.load(Ordering::Relaxed) != self.seen_version;
        // Every 2 s: apps start and quit, devices come and go.
        if changed || self.last_refresh.is_none_or(|t| t.elapsed() >= Duration::from_secs(2)) {
            self.refresh();
        }
        for c in &mut self.children {
            self.scratch.clear();
            c.source.poll(&mut self.scratch, start, now);
            for mut p in self.scratch.drain(..) {
                if c.gain != 1.0 {
                    for v in &mut p.samples {
                        *v *= c.gain;
                    }
                }
                p.stream = c.stream;
                out.push(p);
            }
        }
    }
}

impl Drop for MixSource {
    fn drop(&mut self) {
        if let Ok(mut g) = CAPTURE_STATE.lock() {
            *g = CaptureState::default();
        }
    }
}

/// Sums packets from several sources on one timeline.
///
/// Positions are in audio frames on the recording timeline. Everything
/// before `base` has been handed out already; `buf` holds the frames from
/// `base` on that are still being mixed. Packets of the same stream that
/// arrive within 10 ms of where the previous one ended are treated as
/// contiguous (timestamps jitter a little), so a stream never clicks.
pub struct MixBuffer {
    base: i64,
    buf: VecDeque<f32>,
    expected: HashMap<u32, i64>,
}

/// 10 ms of timestamp jitter is treated as contiguous.
const TOLERANCE: i64 = 480;
/// Packets more than 2 s ahead of the output are dropped (bad timestamps).
const MAX_AHEAD: i64 = 96_000;

impl MixBuffer {
    pub fn new(base: i64) -> MixBuffer {
        MixBuffer { base, buf: VecDeque::new(), expected: HashMap::new() }
    }

    pub fn base(&self) -> i64 {
        self.base
    }

    /// Adds a packet whose first frame belongs at timeline frame `pos`.
    /// Returns how many frames arrived too late to be used.
    pub fn add(&mut self, stream: u32, pos: i64, samples: &[f32]) -> i64 {
        let frames = (samples.len() / CHANNELS) as i64;
        let pos = match self.expected.get(&stream) {
            Some(&e) if (pos - e).abs() <= TOLERANCE => e,
            _ => pos,
        };
        self.expected.insert(stream, pos + frames);
        let late = (self.base - pos).clamp(0, frames);
        let off = pos + late - self.base;
        let n = frames - late;
        if n <= 0 || off + n > MAX_AHEAD {
            return late;
        }
        let need = ((off + n) as usize) * CHANNELS;
        if self.buf.len() < need {
            self.buf.resize(need, 0.0);
        }
        let src = &samples[late as usize * CHANNELS..];
        for (i, v) in src.iter().enumerate() {
            self.buf[off as usize * CHANNELS + i] += v;
        }
        late
    }

    /// Hands out every frame before `target` (silence where nothing
    /// played), clipped to -1..=1.
    pub fn take_until(&mut self, target: i64, out: &mut Vec<f32>) {
        out.clear();
        let n = (target - self.base).max(0) as usize;
        for _ in 0..n * CHANNELS {
            out.push(self.buf.pop_front().unwrap_or(0.0).clamp(-1.0, 1.0));
        }
        self.base += n as i64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize, v: f32) -> Vec<f32> {
        vec![v; frames * CHANNELS]
    }

    #[test]
    fn mixes_two_streams_by_time() {
        let mut m = MixBuffer::new(0);
        m.add(1, 0, &tone(100, 0.25));
        m.add(2, 50, &tone(100, 0.5));
        let mut out = Vec::new();
        m.take_until(200, &mut out);
        assert_eq!(out.len(), 400);
        assert_eq!(out[2 * 10], 0.25, "only stream 1");
        assert_eq!(out[2 * 60], 0.75, "both");
        assert_eq!(out[2 * 120], 0.5, "only stream 2");
        assert_eq!(out[2 * 170], 0.0, "silence");
        assert_eq!(m.base(), 200);
    }

    #[test]
    fn jitter_is_contiguous_and_late_audio_is_trimmed() {
        let mut m = MixBuffer::new(0);
        m.add(1, 0, &tone(100, 0.1));
        // 5 frames of timestamp jitter: placed right after the first packet.
        m.add(1, 105, &tone(100, 0.2));
        let mut out = Vec::new();
        m.take_until(150, &mut out);
        assert_eq!(out[2 * 99], 0.1);
        assert_eq!(out[2 * 100], 0.2, "no gap from jitter");
        // Arrives after its first 30 frames were already handed out.
        let late = m.add(2, 120, &tone(60, 0.3));
        assert_eq!(late, 30);
        m.take_until(200, &mut out);
        assert!((out[0] - 0.5).abs() < 1e-6, "late packet's remainder is mixed in");
        // Loud sums are clipped.
        m.add(3, 200, &tone(10, 0.9));
        m.add(4, 200, &tone(10, 0.9));
        m.take_until(210, &mut out);
        assert_eq!(out[0], 1.0);
    }
}
