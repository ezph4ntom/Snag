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

use eframe::egui::{self, Rect, RichText, Sense, pos2, vec2};

use super::widgets;

use super::{Shared, dim, section};
use crate::audio::devices;
use crate::config::{AudioAppChoice, AudioDeviceChoice, AudioSettings};

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
    /// When the last live volume update was sent to the recorder.
    last_live: Option<Instant>,
}

/// Which setting a source row edits.
#[derive(Clone, PartialEq)]
enum RowId {
    Mic,
    Device(String),
    /// Lower-case exe name.
    App(String),
}

/// One sound source as a row: what it is, its level and its choices.
struct Row {
    id: RowId,
    /// "Microphone", "Output", "Game", "Playing"...
    kind: &'static str,
    name: String,
    peak: f32,
    include: bool,
    volume: f32,
}

/// "Speakers (HyperX Cloud Alpha S Game)" -> ("Speakers", "HyperX Cloud Alpha S Game").
fn split_name(name: &str) -> (&str, &str) {
    match name.split_once(" (") {
        Some((m, rest)) => (m, rest.trim_end_matches(')')),
        None => (name, ""),
    }
}

impl SoundTab {
    /// Called every frame with whether a tab that shows sound levels is
    /// visible, so the level reader only runs while it's needed.
    pub fn set_visible(&mut self, visible: bool, ctx: &egui::Context) {
        match (&self.monitor, visible) {
            (None, true) => self.monitor = Some(Monitor::start(ctx.clone())),
            (Some(m), v) => m.active.store(v, Ordering::Relaxed),
            _ => {}
        }
    }

    fn snapshot(&self) -> Snapshot {
        self.monitor.as_ref().and_then(|m| m.snapshot.lock().ok().map(|g| g.clone())).unwrap_or_default()
    }

    /// The rows to show: the microphone, then every output (whole-PC mode)
    /// or the apps playing sound / ticked before (apps mode).
    fn rows(snap: &Snapshot, a: &AudioSettings) -> Vec<Row> {
        let mut rows = Vec::new();
        let mic = if a.mic_device.is_empty() {
            snap.mics.iter().find(|m| m.is_default)
        } else {
            snap.mics.iter().find(|m| m.id == a.mic_device)
        };
        rows.push(Row {
            id: RowId::Mic,
            kind: "Microphone",
            name: mic.map(|m| m.name.clone()).unwrap_or_else(|| "Microphone".into()),
            peak: mic.map(|m| m.peak).unwrap_or(0.0),
            include: a.record_mic,
            volume: a.mic_volume,
        });
        if a.whole_pc {
            for d in &snap.outputs {
                let choice = a.devices.iter().find(|c| c.id == d.id);
                rows.push(Row {
                    id: RowId::Device(d.id.clone()),
                    kind: if d.is_default { "Output (default)" } else { "Output" },
                    name: d.name.clone(),
                    peak: d.peak,
                    include: choice.is_none_or(|c| c.include),
                    volume: choice.map(|c| c.volume).unwrap_or(1.0),
                });
            }
            return rows;
        }
        let mut apps: Vec<AppLevel> = snap.apps.clone();
        for c in &a.apps {
            if c.include && !apps.iter().any(|l| l.exe == c.exe) {
                apps.push(AppLevel { exe: c.exe.clone(), name: c.name.clone(), ..Default::default() });
            }
        }
        apps.sort_by_key(|x| (!x.playing, !x.game, x.name.to_lowercase()));
        for app in apps {
            let choice = a.apps.iter().find(|c| c.exe == app.exe);
            let running = snap.apps.iter().any(|s| s.exe == app.exe);
            rows.push(Row {
                kind: match (app.game, app.playing, running) {
                    (true, ..) => "Game",
                    (_, true, _) => "Playing",
                    (_, _, true) => "Quiet",
                    _ => "Not running",
                },
                id: RowId::App(app.exe.clone()),
                name: app.name.clone(),
                peak: app.peak,
                include: choice.is_some_and(|c| c.include),
                volume: choice.map(|c| c.volume).unwrap_or(1.0),
            });
        }
        rows
    }

    /// Writes a row's choices back into the settings.
    fn apply(a: &mut AudioSettings, row: &Row, include: bool, volume: f32) {
        match &row.id {
            RowId::Mic => {
                a.record_mic = include;
                a.mic_volume = volume;
            }
            RowId::Device(id) => {
                a.devices.retain(|d| &d.id != id);
                a.devices.push(AudioDeviceChoice { id: id.clone(), name: row.name.clone(), include, volume });
            }
            RowId::App(exe) => {
                a.apps.retain(|x| &x.exe != exe);
                a.apps.push(AudioAppChoice { exe: exe.clone(), name: row.name.clone(), include, volume });
            }
        }
    }

    /// Saves changed choices and tells the recorder right away (at most 30
    /// times a second), so a slider is heard while it's dragged.
    fn commit(&mut self, shared: &mut Shared, a: AudioSettings) {
        if a == shared.settings.audio {
            return;
        }
        if self.last_live.is_none_or(|t| t.elapsed() >= Duration::from_millis(33)) {
            self.last_live = Some(Instant::now());
            shared.outbox.push(crate::app::ipc::UiToCore::LiveAudio { audio: a.clone() });
        }
        shared.settings.audio = a;
        shared.mark_dirty();
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, shared: &mut Shared) {
        let snap = self.snapshot();
        let mut a = shared.settings.audio.clone();
        let status = shared.status.clone().unwrap_or_default();
        let warn = shared.theme.colors.warning.0;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(900.0);
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
                    ui.label(dim(shared, format!("Recording now: {now}")).small());
                }
                if let Some(n) = &status.audio_note {
                    ui.label(RichText::new(n).color(warn).small());
                }
            });
            ui.add_enabled_ui(a.record_pc_audio, |ui| {
                section(ui, shared, "Sources", |ui| {
                    ui.checkbox(&mut a.whole_pc, "Whole PC audio (all outputs)");
                    let hint = if a.whole_pc {
                        "Everything on every output. Untick to pick apps one by one."
                    } else {
                        "Only the ticked apps (and the microphone) are recorded."
                    };
                    ui.label(dim(shared, hint).small());
                    ui.add_space(10.0);
                    let rows = Self::rows(&snap, &a);
                    if rows.len() == 1 && !a.whole_pc {
                        ui.label(dim(shared, "No apps are playing sound right now. Start one and it shows up here."));
                    }
                    for row in &rows {
                        Self::full_row(ui, shared, &snap, &mut a, row);
                    }
                });
            });
        });
        self.commit(shared, a);
    }

    /// A row on the Sound tab: Record box, name, level meter, value, slider.
    fn full_row(ui: &mut egui::Ui, shared: &Shared, snap: &Snapshot, a: &mut AudioSettings, row: &Row) {
        let theme = shared.theme.clone();
        let c = &theme.colors;
        let (mut include, mut volume) = (row.include, row.volume);
        ui.horizontal(|ui| {
            ui.set_min_height(48.0);
            ui.checkbox(&mut include, "").on_hover_text("Record this source");
            let (main, sub) = split_name(&row.name);
            ui.allocate_ui_with_layout(vec2(250.0, 44.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_min_width(250.0);
                ui.set_max_width(250.0);
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.add(egui::Label::new(RichText::new(main).strong()).truncate()).on_hover_text(&row.name);
                let detail = if sub.is_empty() { row.kind.to_string() } else { format!("{} · {sub}", row.kind) };
                ui.add(egui::Label::new(RichText::new(detail).small().color(c.text_dim.0)).truncate());
            });
            meter(ui, &theme, row.peak * volume, include, 90.0);
            ui.add_space(6.0);
            ui.add_enabled_ui(include, |ui| {
                widgets::value_box(ui, &theme, &format!("{}", (volume * 100.0).round()), "%", 64.0);
                if widgets::pill_slider(ui, &theme, &mut volume, 200.0).changed() {
                    volume = (volume * 100.0).round() / 100.0;
                }
            });
            if row.id == RowId::Mic {
                let mut device = a.mic_device.clone();
                let text = if device.is_empty() { "Default mic".to_string() } else { "Chosen mic".to_string() };
                egui::ComboBox::from_id_salt("mic_pick").selected_text(text).width(120.0).show_ui(ui, |ui| {
                    ui.selectable_value(&mut device, String::new(), "Windows default");
                    for m in &snap.mics {
                        ui.selectable_value(&mut device, m.id.clone(), &m.name);
                    }
                });
                a.mic_device = device;
            }
        });
        if include != row.include || volume != row.volume {
            Self::apply(a, row, include, volume);
        }
        ui.separator();
    }

    /// The Live tab's "Sound in clips": the recorded sources with a volume
    /// slider each.
    pub fn compact(&mut self, ui: &mut egui::Ui, shared: &mut Shared) {
        let snap = self.snapshot();
        let mut a = shared.settings.audio.clone();
        let theme = shared.theme.clone();
        if !a.record_pc_audio {
            ui.label(dim(shared, "Sound is off (Sound tab)."));
            return;
        }
        let rows: Vec<Row> = Self::rows(&snap, &a).into_iter().filter(|r| r.include).collect();
        if rows.is_empty() {
            ui.label(dim(shared, "Nothing is recorded. Pick sources in the Sound tab."));
        }
        let w = ui.available_width();
        for row in &rows {
            let (main, _) = split_name(&row.name);
            let mut volume = row.volume;
            ui.horizontal(|ui| {
                ui.add(egui::Label::new(RichText::new(main)).truncate());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(format!("{} %", (volume * 100.0).round())).color(theme.colors.text_dim.0));
                });
            });
            if widgets::pill_slider(ui, &theme, &mut volume, w).changed() {
                volume = (volume * 100.0).round() / 100.0;
                Self::apply(&mut a, row, true, volume);
            }
            ui.add_space(8.0);
        }
        self.commit(shared, a);
    }
}

/// A thin horizontal level meter (-60 dB left, 0 dB right).
fn meter(ui: &mut egui::Ui, theme: &crate::themes::Theme, peak: f32, on: bool, width: f32) {
    let c = &theme.colors;
    let (rect, _) = ui.allocate_exact_size(vec2(width, 6.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 3.0, c.control.map(|h| h.0).unwrap_or(c.widget.0));
    let level = if peak <= 0.001 { 0.0 } else { ((20.0 * peak.log10() + 60.0) / 60.0).clamp(0.0, 1.0) };
    if level > 0.0 {
        let color = if level > 0.92 { c.error.0 } else if level > 0.75 { c.warning.0 } else { c.success.0 };
        let fill = Rect::from_min_max(rect.min, pos2(rect.left() + rect.width() * level, rect.bottom()));
        p.rect_filled(fill, 3.0, if on { color } else { color.gamma_multiply(0.35) });
    }
}
