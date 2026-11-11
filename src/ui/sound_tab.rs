//! Sound tab: what goes into a clip's audio track, as a small mixer.
//!
//! * a strip for the microphone (your voice), on in either mode;
//! * "Whole PC audio": a strip per playback device (the HyperX "Game" and
//!   "Chat" devices are separate strips, both recorded);
//! * otherwise a strip per app that is playing audio (or was recently, or
//!   was ticked before): tick the ones to record. Choices are remembered by
//!   exe name and applied live, without restarting the replay buffer.
//!
//! Levels come from Windows' own peak meters (the same ones the volume
//! mixer shows), read by a background thread only while this tab is open.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Rect, RichText, Sense, pos2, vec2};

use super::{Shared, dim, section};
use crate::audio::devices;
use crate::config::{AudioAppChoice, AudioDeviceChoice};

#[derive(Clone, Default)]
struct Level {
    id: String,
    name: String,
    is_default: bool,
    peak: f32,
}

#[derive(Clone, Default)]
struct AppLevel {
    exe: String,
    name: String,
    game: bool,
    playing: bool,
    peak: f32,
}

#[derive(Clone, Default)]
struct Snapshot {
    outputs: Vec<Level>,
    mics: Vec<Level>,
    apps: Vec<AppLevel>,
}

struct Monitor {
    snapshot: Arc<Mutex<Snapshot>>,
    active: Arc<AtomicBool>,
}

impl Monitor {
    fn start(ctx: egui::Context) -> Monitor {
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let active = Arc::new(AtomicBool::new(true));
        let (snap, act) = (snapshot.clone(), active.clone());
        std::thread::Builder::new()
            .name("sound-levels".into())
            .spawn(move || monitor_main(snap, act, ctx))
            .ok();
        Monitor { snapshot, active }
    }
}

/// Re-lists devices and apps every second and reads their meters 20 times
/// a second, while the tab is visible.
fn monitor_main(out: Arc<Mutex<Snapshot>>, active: Arc<AtomicBool>, ctx: egui::Context) {
    use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
    // SAFETY: COM for this thread (the meter objects live here only).
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
    }
    let mut outputs: Vec<(Level, Option<IAudioMeterInformation>)> = Vec::new();
    let mut mics: Vec<(Level, Option<IAudioMeterInformation>)> = Vec::new();
    // exe -> (info, meters of its sessions, last time it had a session)
    let mut apps: HashMap<String, (AppLevel, Vec<IAudioMeterInformation>, Instant)> = HashMap::new();
    let mut names: HashMap<u32, (String, String, bool)> = HashMap::new(); // pid -> exe, name, game
    let mut last_list: Option<Instant> = None;
    let decay = |old: f32, new: f32| new.max(old * 0.8);
    loop {
        if !active.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(200));
            last_list = None;
            continue;
        }
        if last_list.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
            last_list = Some(Instant::now());
            let lv = |d: devices::RenderDevice| {
                let m = devices::device_meter(&d.id);
                (Level { id: d.id, name: d.name, is_default: d.is_default, peak: 0.0 }, m)
            };
            outputs = devices::render_devices().unwrap_or_default().into_iter().map(lv).collect();
            mics = devices::capture_devices().unwrap_or_default().into_iter().map(lv).collect();
            for (_, meters, _) in apps.values_mut() {
                meters.clear();
            }
            for s in devices::sessions() {
                let (exe, name, game) = names
                    .entry(s.pid)
                    .or_insert_with(|| match crate::win::process_exe_path(s.pid) {
                        Some(p) => (devices::exe_key(&p), devices::app_display_name(&p), devices::looks_like_game(&p)),
                        None => (String::new(), String::new(), false),
                    })
                    .clone();
                if exe.is_empty() {
                    continue;
                }
                let e = apps.entry(exe.clone()).or_insert_with(|| {
                    (AppLevel { exe, name, game, ..Default::default() }, Vec::new(), Instant::now())
                });
                e.2 = Instant::now();
                if s.active {
                    e.0.playing = true;
                }
                e.1.extend(s.meter);
            }
            // Apps that stopped a minute ago drop off the list (unless ticked;
            // the tab adds those back from settings).
            apps.retain(|_, (_, _, seen)| seen.elapsed() < Duration::from_secs(60));
            for (a, meters, _) in apps.values_mut() {
                if meters.is_empty() {
                    a.playing = false;
                }
            }
        }
        let mut snap = Snapshot::default();
        for (l, m) in outputs.iter_mut().chain(mics.iter_mut()) {
            l.peak = decay(l.peak, m.as_ref().map(devices::peak).unwrap_or(0.0));
        }
        snap.outputs = outputs.iter().map(|(l, _)| l.clone()).collect();
        snap.mics = mics.iter().map(|(l, _)| l.clone()).collect();
        for (a, meters, _) in apps.values_mut() {
            let p = meters.iter().map(devices::peak).fold(0.0f32, f32::max);
            a.peak = decay(a.peak, p);
            if p > 0.001 {
                a.playing = true;
            }
            snap.apps.push(a.clone());
        }
        if let Ok(mut g) = out.lock() {
            *g = snap;
        }
        ctx.request_repaint();
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[derive(Default)]
pub struct SoundTab {
    monitor: Option<Monitor>,
}

/// One mixer strip's result.
struct StripOut {
    include: bool,
    volume: f32,
}

fn db(peak: f32) -> String {
    if peak <= 0.000_01 { "-inf dB".into() } else { format!("{:.1} dB", 20.0 * peak.log10()) }
}

impl SoundTab {
    /// Called every frame with whether the tab is visible, so the level
    /// reader only runs while it's needed.
    pub fn set_visible(&mut self, visible: bool, ctx: &egui::Context) {
        match (&self.monitor, visible) {
            (None, true) => self.monitor = Some(Monitor::start(ctx.clone())),
            (Some(m), v) => m.active.store(v, Ordering::Relaxed),
            _ => {}
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, shared: &mut Shared) {
        let snap = self.monitor.as_ref().and_then(|m| m.snapshot.lock().ok().map(|g| g.clone())).unwrap_or_default();
        let mut a = shared.settings.audio.clone();
        let status = shared.status.clone().unwrap_or_default();
        let warn = shared.theme.colors.warning.0;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            section(ui, shared, "Recording", |ui| {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut a.record_pc_audio, "Record sound in clips");
                    ui.add_enabled_ui(a.record_pc_audio, |ui| {
                        egui::ComboBox::from_id_salt("audio_kbps")
                            .selected_text(format!("{} kbps AAC", a.bitrate_kbps))
                            .show_ui(ui, |ui| {
                                for k in [128, 160, 192, 256, 320] {
                                    ui.selectable_value(&mut a.bitrate_kbps, k, format!("{k} kbps AAC"));
                                }
                            });
                    });
                });
                if shared.connected && status.recording && status.audio {
                    let now = if status.audio_sources.is_empty() { "nothing".into() } else { status.audio_sources.join(", ") };
                    ui.label(dim(shared, format!("Recording now: {now}")));
                }
                if let Some(n) = &status.audio_note {
                    ui.label(RichText::new(n).color(warn).small());
                }
            });
            ui.add_enabled_ui(a.record_pc_audio, |ui| {
                section(ui, shared, "Sources", |ui| {
                    ui.checkbox(&mut a.whole_pc, "Whole PC audio (all outputs)");
                    ui.label(dim(shared, if a.whole_pc {
                        "Everything on every output. Untick to pick apps one by one."
                    } else {
                        "Only the ticked apps (and the microphone) are recorded."
                    }).small());
                    ui.add_space(6.0);
                    egui::ScrollArea::horizontal().id_salt("strips").show(ui, |ui| {
                        ui.horizontal_top(|ui| {
                            self.mic_strip(ui, shared, &snap, &mut a);
                            if a.whole_pc {
                                for d in &snap.outputs {
                                    let choice = a.devices.iter().find(|c| c.id == d.id).cloned().unwrap_or(AudioDeviceChoice {
                                        id: d.id.clone(),
                                        name: d.name.clone(),
                                        ..Default::default()
                                    });
                                    let label = if d.is_default { "Output (default)" } else { "Output" };
                                    let r = strip(ui, shared, ("dev", &d.id), label, &d.name, d.peak, choice.include, choice.volume, None);
                                    if r.include != choice.include || r.volume != choice.volume {
                                        let mut c = choice.clone();
                                        c.include = r.include;
                                        c.volume = r.volume;
                                        a.devices.retain(|x| x.id != c.id);
                                        a.devices.push(c);
                                    }
                                }
                            } else {
                                self.app_strips(ui, shared, &snap, &mut a);
                            }
                        });
                    });
                });
            });
        });
        if a != shared.settings.audio {
            shared.settings.audio = a;
            shared.mark_dirty();
        }
    }

    fn mic_strip(&mut self, ui: &mut egui::Ui, shared: &Shared, snap: &Snapshot, a: &mut crate::config::AudioSettings) {
        let current = if a.mic_device.is_empty() {
            snap.mics.iter().find(|m| m.is_default)
        } else {
            snap.mics.iter().find(|m| m.id == a.mic_device)
        };
        let name = current.map(|m| m.name.clone()).unwrap_or_else(|| "No microphone".into());
        let peak = current.map(|m| m.peak).unwrap_or(0.0);
        let mut device = a.mic_device.clone();
        let mics = &snap.mics;
        let mut pick = |ui: &mut egui::Ui| {
            let text = if device.is_empty() { "Default" } else { "Chosen" };
            egui::ComboBox::from_id_salt("mic_pick").selected_text(text).width(96.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut device, String::new(), "Windows default");
                for m in mics {
                    ui.selectable_value(&mut device, m.id.clone(), &m.name);
                }
            });
        };
        let r = strip(ui, shared, ("mic", ""), "Microphone", &name, peak, a.record_mic, a.mic_volume, Some(&mut pick));
        a.mic_device = device;
        a.record_mic = r.include;
        a.mic_volume = r.volume;
    }

    fn app_strips(&mut self, ui: &mut egui::Ui, shared: &Shared, snap: &Snapshot, a: &mut crate::config::AudioSettings) {
        // Apps seen now, plus remembered ticked apps that aren't running.
        let mut list: Vec<AppLevel> = snap.apps.clone();
        for c in &a.apps {
            if c.include && !list.iter().any(|l| l.exe == c.exe) {
                list.push(AppLevel { exe: c.exe.clone(), name: c.name.clone(), ..Default::default() });
            }
        }
        list.sort_by_key(|x| (!x.playing, !x.game, x.name.to_lowercase()));
        if list.is_empty() {
            ui.label(dim(shared, "No apps are playing sound right now. Start one and it shows up here."));
        }
        for app in list {
            let choice = a.apps.iter().find(|c| c.exe == app.exe).cloned().unwrap_or(AudioAppChoice {
                exe: app.exe.clone(),
                name: app.name.clone(),
                include: false,
                volume: 1.0,
            });
            let kind = match (app.game, app.playing) {
                (true, _) => "Game",
                (false, true) => "Playing",
                (false, false) if app.peak == 0.0 && !snap.apps.iter().any(|s| s.exe == app.exe) => "Not running",
                _ => "Quiet",
            };
            let r = strip(ui, shared, ("app", &app.exe), kind, &app.name, app.peak, choice.include, choice.volume, None);
            if r.include != choice.include || r.volume != choice.volume {
                let mut c = choice;
                c.include = r.include;
                c.volume = r.volume;
                c.name = app.name.clone();
                a.apps.retain(|x| x.exe != c.exe);
                a.apps.push(c);
            }
        }
    }
}

/// One vertical mixer strip: kind, name, level in dB, volume fader next to
/// a peak meter, and the "Record" box. `extra` adds a widget (mic picker).
#[allow(clippy::too_many_arguments)]
fn strip(
    ui: &mut egui::Ui,
    shared: &Shared,
    id: (&str, &str),
    kind: &str,
    name: &str,
    peak: f32,
    include: bool,
    volume: f32,
    extra: Option<&mut dyn FnMut(&mut egui::Ui)>,
) -> StripOut {
    let c = &shared.theme.colors;
    let mut out = StripOut { include, volume };
    egui::Frame::new()
        .fill(c.background.0)
        .stroke(egui::Stroke::new(1.0, if include { c.accent.0 } else { c.border.0 }))
        .corner_radius(egui::CornerRadius::same(shared.theme.corner_radius.clamp(0.0, 255.0) as u8))
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.push_id(id, |ui| {
                ui.set_width(112.0);
                ui.set_min_height(262.0);
                ui.vertical_centered(|ui| {
                    let kind_color = if include { c.accent.0 } else { c.text_dim.0 };
                    ui.label(RichText::new(kind).small().color(kind_color));
                    // "Speakers (HyperX Cloud Alpha S Game)": the part in
                    // brackets goes on its own smaller line.
                    let (main, sub) = match name.split_once(" (") {
                        Some((m, rest)) => (m, rest.trim_end_matches(')')),
                        None => (name, ""),
                    };
                    ui.add(egui::Label::new(RichText::new(main).strong()).truncate()).on_hover_text(name);
                    ui.add(egui::Label::new(RichText::new(if sub.is_empty() { " " } else { sub }).small().color(c.text_dim.0)).truncate())
                        .on_hover_text(name);
                    ui.label(RichText::new(db(peak * volume)).small().color(c.text_dim.0).monospace());
                    ui.horizontal(|ui| {
                        ui.add_space(14.0);
                        let mut pct = (volume * 100.0).round() as u32;
                        ui.spacing_mut().slider_width = 130.0;
                        let resp = ui
                            .add(egui::Slider::new(&mut pct, 0..=100).vertical().show_value(false))
                            .on_hover_text(format!("Volume in clips: {pct} %"));
                        if resp.changed() {
                            out.volume = pct as f32 / 100.0;
                        }
                        ui.add_space(6.0);
                        meter(ui, shared, peak * volume, include);
                    });
                    ui.label(RichText::new(format!("{} %", (out.volume * 100.0).round())).small());
                    if let Some(f) = extra {
                        f(ui);
                    }
                    ui.checkbox(&mut out.include, "Record");
                });
            });
        });
    out
}

/// A vertical peak meter from -60 dB (bottom) to 0 dB (top).
fn meter(ui: &mut egui::Ui, shared: &Shared, peak: f32, on: bool) {
    let c = &shared.theme.colors;
    let (rect, _) = ui.allocate_exact_size(vec2(10.0, 130.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 2.0, c.timeline.0);
    let level = if peak <= 0.001 { 0.0 } else { ((20.0 * peak.log10() + 60.0) / 60.0).clamp(0.0, 1.0) };
    let top = rect.bottom() - rect.height() * level;
    let segments = [(0.0, 0.7, c.success.0), (0.7, 0.9, c.warning.0), (0.9, 1.0, c.error.0)];
    for (from, to, color) in segments {
        let y0 = rect.bottom() - rect.height() * from;
        let y1 = (rect.bottom() - rect.height() * to).max(top);
        if y1 < y0 {
            let col = if on { color } else { color.gamma_multiply(0.35) };
            p.rect_filled(Rect::from_min_max(pos2(rect.left(), y1), pos2(rect.right(), y0)), 1.0, col);
        }
    }
    // Ticks every 12 dB.
    for i in 1..5 {
        let y = rect.bottom() - rect.height() * i as f32 / 5.0;
        p.line_segment([pos2(rect.left(), y), pos2(rect.right(), y)], egui::Stroke::new(1.0, Color32::from_black_alpha(90)));
    }
}
