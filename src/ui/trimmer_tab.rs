//! Clips tab: clip list + trimmer (preview, timeline with draggable start/end
//! handles, frame stepping, fast/precise export).
//!
//! Keyboard: Space play/pause · ←/→ one frame · Shift+←/→ one second ·
//! I / O set start / end at the playhead · Home / End jump to start / end.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use eframe::egui::{self, Color32, CursorIcon, Key, Pos2, Rect, RichText, Sense, Stroke, TextureHandle, TextureOptions, pos2, vec2};

use super::{Shared, dim};
use crate::config::Settings;
use crate::trimmer::audio::AudioPlayer;
use crate::trimmer::player::{Decoder, Frame, preview_size};
use crate::trimmer::{ClipProbe, Output, TrimJob, TrimMode, effective_start, probe, trim};

const CACHE_FRAMES: usize = 24;

struct ClipEntry {
    path: PathBuf,
    name: String,
    size: u64,
    modified: SystemTime,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Drag {
    Start,
    End,
    Playhead,
}

struct Loaded {
    probe: Arc<ClipProbe>,
    size: (u32, u32),
    start: usize,
    end: usize,
    playhead: usize,
    shown: Option<usize>,
    texture: Option<TextureHandle>,
    decoder: Option<Decoder>,
    decoder_next: usize,
    decoder_started: Instant,
    /// Recently decoded frames, so stepping backwards is instant.
    cache: VecDeque<Frame>,
    /// Playing since (instant, frame at that instant).
    playing: Option<(Instant, usize)>,
    /// The clip's sound while playing (follows `playing`).
    audio: Option<AudioPlayer>,
    drag: Option<Drag>,
    /// Last time the playhead moved; an idle decoder is stopped after a few
    /// seconds so a paused clip holds no ffmpeg process (or its memory).
    last_activity: Instant,
}

const DECODER_IDLE: Duration = Duration::from_secs(3);

enum ExportMsg {
    Progress(f32),
    Done(Result<PathBuf, String>),
}

struct Export {
    rx: Receiver<ExportMsg>,
    progress: f32,
    cancel: Arc<AtomicBool>,
    overwrite: bool,
}

pub struct TrimmerTab {
    clips_dir: PathBuf,
    clips: Vec<ClipEntry>,
    last_scan: Option<Instant>,
    loaded: Option<Loaded>,
    loading: Option<(PathBuf, Receiver<Result<ClipProbe, String>>)>,
    precise: bool,
    export: Option<Export>,
    message: Option<(String, bool, Option<PathBuf>)>,
    confirm_overwrite: bool,
    open_dialog: Option<Receiver<PathBuf>>,
}

fn fmt_time(t: f64) -> String {
    let t = t.max(0.0);
    let m = (t / 60.0).floor();
    format!("{:02}:{:06.3}", m as u32, t - m * 60.0)
}

fn fmt_size(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        format!("{:.1} GB", bytes as f64 / (1u64 << 30) as f64)
    } else {
        format!("{:.1} MB", bytes as f64 / (1u64 << 20) as f64)
    }
}

impl TrimmerTab {
    pub fn new(settings: &Settings) -> Self {
        TrimmerTab {
            clips_dir: settings.clips_dir.clone(),
            clips: Vec::new(),
            last_scan: None,
            loaded: None,
            loading: None,
            precise: settings.trimmer.precise_by_default,
            export: None,
            message: None,
            confirm_overwrite: false,
            open_dialog: None,
        }
    }

    /// Starts loading a clip (ffprobe runs on a worker thread).
    pub fn load(&mut self, path: PathBuf) {
        let (tx, rx) = channel();
        let p = path.clone();
        std::thread::spawn(move || {
            let _ = tx.send(probe(&p).map_err(|e| format!("{e:#}")));
        });
        self.loading = Some((path, rx));
    }

    /// Work that must happen even while the window is minimized.
    pub fn poll_background(&mut self, settings: &Settings) {
        if settings.clips_dir != self.clips_dir {
            self.clips_dir = settings.clips_dir.clone();
            self.last_scan = None;
        }
        if self.last_scan.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
            self.scan();
        }
        if let Some((path, rx)) = &self.loading {
            if let Ok(result) = rx.try_recv() {
                match result {
                    Ok(p) if p.frame_count() > 0 => {
                        let n = p.frame_count();
                        self.loaded = Some(Loaded {
                            size: preview_size(p.width, p.height),
                            probe: Arc::new(p),
                            start: 0,
                            end: n - 1,
                            playhead: 0,
                            shown: None,
                            texture: None,
                            decoder: None,
                            decoder_next: 0,
                            decoder_started: Instant::now(),
                            cache: VecDeque::new(),
                            playing: None,
                            audio: None,
                            drag: None,
                            last_activity: Instant::now(),
                        });
                        self.message = None;
                    }
                    Ok(_) => self.message = Some((format!("{} has no frames", path.display()), true, None)),
                    Err(e) => self.message = Some((format!("Can't open {}: {e}", path.display()), true, None)),
                }
                self.loading = None;
            }
        }
        if let Some(l) = self.loaded.as_mut() {
            if l.playing.is_none() && l.decoder.is_some() && l.last_activity.elapsed() > DECODER_IDLE {
                l.decoder = None;
            }
        }
        if let Some(ex) = &mut self.export {
            let mut done = None;
            while let Ok(msg) = ex.rx.try_recv() {
                match msg {
                    ExportMsg::Progress(p) => ex.progress = p,
                    ExportMsg::Done(r) => done = Some(r),
                }
            }
            if let Some(r) = done {
                let overwrite = ex.overwrite;
                self.export = None;
                match r {
                    Ok(path) => {
                        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                        self.message = Some((format!("Saved {name}"), false, Some(path.clone())));
                        self.scan();
                        if overwrite {
                            self.load(path);
                        }
                    }
                    Err(e) => self.message = Some((format!("Export failed: {e}"), true, None)),
                }
            }
        }
    }

    fn scan(&mut self) {
        self.last_scan = Some(Instant::now());
        let mut clips: Vec<ClipEntry> = std::fs::read_dir(&self.clips_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                let name = path.file_name()?.to_string_lossy().to_string();
                let ext_ok = path.extension().is_some_and(|x| ["mp4", "mkv", "mov"].iter().any(|v| x.eq_ignore_ascii_case(v)));
                if !ext_ok || name.starts_with('.') {
                    return None; // skip temp/partial files
                }
                let meta = e.metadata().ok()?;
                Some(ClipEntry { path, name, size: meta.len(), modified: meta.modified().ok()? })
            })
            .collect();
        clips.sort_by_key(|c| std::cmp::Reverse(c.modified));
        clips.truncate(300);
        self.clips = clips;
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, shared: &mut Shared) {
        if let Some(rx) = &self.open_dialog {
            if let Ok(p) = rx.try_recv() {
                self.open_dialog = None;
                self.load(p);
            }
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
        if self.loading.is_some() || self.export.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(30));
        }

        egui::Panel::left("clip_list")
            .resizable(true)
            .default_size(270.0)
            .frame(egui::Frame::new().inner_margin(egui::Margin { right: 10, ..Default::default() }))
            .show(ui, |ui| self.clip_list(ui, shared));

        egui::CentralPanel::no_frame().show(ui, |ui| {
            if self.loaded.is_some() {
                self.editor(ui, shared);
            } else if self.loading.is_some() {
                ui.centered_and_justified(|ui| ui.spinner());
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label(dim(
                        shared,
                        format!(
                            "Pick a clip on the left, or press {} while playing to save one.",
                            shared.settings.hotkeys.clip
                        ),
                    ));
                });
            }
        });
        self.overwrite_dialog(ui.ctx(), shared);
    }

    fn clip_list(&mut self, ui: &mut egui::Ui, shared: &Shared) {
        ui.horizontal(|ui| {
            ui.heading("Clips");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Folder").on_hover_text("Open the clips folder").clicked() {
                    crate::win::reveal_in_explorer(&self.clips_dir);
                }
                if ui.button("Open file...").clicked() {
                    self.open_dialog = Some(super::dialogs::pick_video(&self.clips_dir));
                }
            });
        });
        ui.separator();
        let current = self.loaded.as_ref().map(|l| l.probe.path.clone());
        let mut clicked = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            if self.clips.is_empty() {
                ui.label(dim(shared, format!("No clips yet in\n{}", self.clips_dir.display())));
            }
            for c in &self.clips {
                let selected = current.as_deref() == Some(c.path.as_path());
                let age = c.modified.elapsed().map(|d| d.as_secs()).unwrap_or(0);
                let when = match age {
                    0..=59 => "just now".to_string(),
                    60..=3599 => format!("{} min ago", age / 60),
                    3600..=86_399 => format!("{} h ago", age / 3600),
                    _ => format!("{} days ago", age / 86_400),
                };
                let text = format!("{}\n{} · {}", c.name, fmt_size(c.size), when);
                if ui.selectable_label(selected, text).clicked() {
                    clicked = Some(c.path.clone());
                }
            }
        });
        if let Some(p) = clicked {
            self.load(p);
        }
    }

    fn editor(&mut self, ui: &mut egui::Ui, shared: &mut Shared) {
        let mode = if self.precise { TrimMode::Precise } else { TrimMode::Fast };
        let exporting_overwrite = self.export.as_ref().is_some_and(|e| e.overwrite);
        let ctx = ui.ctx().clone();
        let typing = ctx.memory(|m| m.focused().is_some());
        let colors = shared.theme.colors.clone();
        let l = self.loaded.as_mut().expect("checked by caller");

        if !typing {
            handle_keys(&ctx, l, mode);
        }
        if !exporting_overwrite {
            pump(&ctx, l);
        }
        sync_audio(l, shared.settings.trimmer.preview_volume);

        // ---- preview -------------------------------------------------------
        let controls_h = 215.0; // room for the transport row to wrap on narrow windows
        let avail = ui.available_size();
        let area = vec2(avail.x, (avail.y - controls_h).max(120.0));
        let (w, h) = (l.size.0 as f32, l.size.1 as f32);
        let scale = (area.x / w).min(area.y / h);
        let img_size = vec2(w * scale, h * scale);
        let (rect, _) = ui.allocate_exact_size(area, Sense::hover());
        let img_rect = Rect::from_center_size(rect.center(), img_size);
        ui.painter().rect_filled(img_rect, 0.0, Color32::BLACK);
        if let Some(tex) = &l.texture {
            ui.painter().image(tex.id(), img_rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        } else {
            ui.put(img_rect, egui::Spinner::new());
        }

        // ---- transport -------------------------------------------------------
        let p = l.probe.clone();
        let n = p.frame_count();
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            if ui.button("|<").on_hover_text("Go to start handle (Home)").clicked() {
                stop_and_seek(l, effective_start(&p, l.start, mode));
            }
            if ui.button("< Frame").on_hover_text("Previous frame (Left arrow; Shift: 1 s)").clicked() {
                stop_and_seek(l, l.playhead.saturating_sub(1));
            }
            let play_label = if l.playing.is_some() { "Pause" } else { "Play" };
            if ui.add(egui::Button::new(play_label).min_size(vec2(80.0, 0.0))).on_hover_text("Space").clicked() {
                toggle_play(l, mode);
            }
            if ui.button("Frame >").on_hover_text("Next frame (Right arrow; Shift: 1 s)").clicked() {
                stop_and_seek(l, (l.playhead + 1).min(n - 1));
            }
            if ui.button(">|").on_hover_text("Go to end handle (End)").clicked() {
                stop_and_seek(l, l.end);
            }
            ui.separator();
            if ui.button("Set start [I]").clicked() {
                l.start = l.playhead.min(l.end.saturating_sub(1));
            }
            if ui.button("Set end [O]").clicked() {
                l.end = l.playhead.max(l.start + 1).min(n - 1);
            }
            ui.separator();
            if p.audio_tracks == 0 {
                ui.label(dim(shared, "No sound in this clip"));
            } else {
                let mut pct = (shared.settings.trimmer.preview_volume * 100.0).round() as u32;
                ui.label("Sound").on_hover_text("Preview volume (doesn't change the clip)");
                ui.spacing_mut().slider_width = 90.0;
                if ui.add(egui::Slider::new(&mut pct, 0..=100).suffix(" %")).changed() {
                    shared.settings.trimmer.preview_volume = pct as f32 / 100.0;
                    shared.mark_dirty();
                }
            }
            ui.separator();
            // Never split mid-text; on a narrow window it moves down whole.
            ui.add(
                egui::Label::new(
                    RichText::new(format!(
                        "{} / {}",
                        fmt_time(p.time_of(l.playhead)),
                        fmt_time(p.duration())
                    ))
                    .monospace(),
                )
                .wrap_mode(egui::TextWrapMode::Extend),
            );
        });

        // ---- timeline ----------------------------------------------------------
        ui.add_space(4.0);
        timeline(ui, l, &colors, mode, shared.theme.corner_radius);

        // ---- selection info + export -----------------------------------------
        let eff = effective_start(&p, l.start, mode);
        let kept_frames = l.end - eff + 1;
        ui.horizontal_wrapped(|ui| {
            ui.label(format!(
                "Start {}   End {}   Length {}",
                fmt_time(p.time_of(eff)),
                fmt_time(p.time_of(l.end) + p.frame_duration()),
                fmt_time(kept_frames as f64 * p.frame_duration())
            ));
            if mode == TrimMode::Fast && eff != l.start {
                ui.label(dim(shared, "(start snapped back to a keyframe)"));
            }
        });
        ui.add_space(4.0);
        let mut start_export = None;
        ui.horizontal(|ui| {
            ui.label("Mode:");
            ui.radio_value(&mut self.precise, false, "Fast")
                .on_hover_text("Lossless and instant. The start snaps to the previous keyframe (one per second).");
            ui.radio_value(&mut self.precise, true, "Precise")
                .on_hover_text("Frame-exact start. Re-encodes with your encoder at high bitrate (takes a few seconds).");
            ui.separator();
            let busy = self.export.is_some();
            if ui.add_enabled(!busy, egui::Button::new("Save as new file")).clicked() {
                start_export = Some(false);
            }
            if ui.add_enabled(!busy, egui::Button::new("Overwrite original...")).clicked() {
                self.confirm_overwrite = true;
            }
            if ui.button("Show in folder").clicked() {
                crate::win::reveal_in_explorer(&p.path);
            }
        });
        if let Some(ex) = &self.export {
            ui.horizontal(|ui| {
                ui.add(egui::ProgressBar::new(ex.progress).desired_width(320.0).show_percentage());
                if ui.button("Cancel").clicked() {
                    ex.cancel.store(true, Ordering::Relaxed);
                }
            });
        }
        if let Some((msg, is_err, path)) = &self.message {
            let col = if *is_err { colors.error.0 } else { colors.success.0 };
            let mut reveal = None;
            ui.horizontal(|ui| {
                ui.label(RichText::new(msg).color(col));
                if let Some(p) = path {
                    if ui.small_button("Show").clicked() {
                        reveal = Some(p.clone());
                    }
                }
            });
            if let Some(p) = reveal {
                crate::win::reveal_in_explorer(&p);
            }
        }
        if let Some(overwrite) = start_export {
            self.start_export(overwrite, shared);
        }
    }

    fn overwrite_dialog(&mut self, ctx: &egui::Context, shared: &mut Shared) {
        if !self.confirm_overwrite {
            return;
        }
        let name = self
            .loaded
            .as_ref()
            .map(|l| l.probe.path.file_name().unwrap_or_default().to_string_lossy().to_string())
            .unwrap_or_default();
        let mut choice = None;
        egui::Modal::new(egui::Id::unique("confirm_overwrite")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.heading("Overwrite original?");
            ui.label(format!("{name} will be replaced by the trimmed version. This can't be undone."));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Overwrite").clicked() {
                    choice = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(false);
                }
            });
        });
        if let Some(yes) = choice {
            self.confirm_overwrite = false;
            if yes {
                self.start_export(true, shared);
            }
        }
    }

    fn start_export(&mut self, overwrite: bool, shared: &Shared) {
        let Some(l) = self.loaded.as_mut() else { return };
        l.playing = None;
        if overwrite {
            // ffmpeg's preview decoder holds the file open; Windows won't
            // replace a file that's open, so release it first.
            l.decoder = None;
        }
        let job = TrimJob {
            start: l.start,
            end: l.end,
            mode: if self.precise { TrimMode::Precise } else { TrimMode::Fast },
            output: if overwrite { Output::Overwrite } else { Output::NewFile },
            encoder: shared.settings.video.encoder,
        };
        let probe = l.probe.clone();
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        std::thread::spawn(move || {
            let ptx = tx.clone();
            let result = trim(&probe, &job, &|p| {
                let _ = ptx.send(ExportMsg::Progress(p));
            }, &c);
            let _ = tx.send(ExportMsg::Done(result.map_err(|e| format!("{e:#}"))));
        });
        self.message = None;
        self.export = Some(Export { rx, progress: 0.0, cancel, overwrite });
    }
}

/// Starts the clip's sound when playback starts (or restarts from another
/// frame) and stops it when playback stops. The player lines itself up with
/// the instant playback started, like the video does.
fn sync_audio(l: &mut Loaded, volume: f32) {
    match l.playing {
        Some((t0, f0)) if l.probe.audio_tracks > 0 => {
            if l.audio.as_ref().is_none_or(|a| a.anchor != t0) {
                let from = l.probe.audio_seek_time(f0);
                l.audio = Some(AudioPlayer::start(&l.probe.path, from, t0, volume));
            }
            if let Some(a) = &l.audio {
                a.set_volume(volume);
            }
        }
        _ => l.audio = None,
    }
}

fn stop_and_seek(l: &mut Loaded, frame: usize) {
    l.playing = None;
    l.playhead = frame.min(l.probe.frame_count().saturating_sub(1));
}

fn toggle_play(l: &mut Loaded, mode: TrimMode) {
    if l.playing.is_some() {
        l.playing = None;
    } else {
        // At (or past) the end handle: restart from the start handle.
        if l.playhead >= l.end {
            l.playhead = effective_start(&l.probe, l.start, mode);
        }
        l.playing = Some((Instant::now(), l.playhead));
    }
}

fn handle_keys(ctx: &egui::Context, l: &mut Loaded, mode: TrimMode) {
    let n = l.probe.frame_count();
    let sec = l.probe.fps.round().max(1.0) as usize;
    ctx.input(|i| {
        let shift = i.modifiers.shift;
        let step = if shift { sec } else { 1 };
        if i.key_pressed(Key::Space) {
            toggle_play(l, mode);
        }
        if i.key_pressed(Key::ArrowLeft) {
            stop_and_seek(l, l.playhead.saturating_sub(step));
        }
        if i.key_pressed(Key::ArrowRight) {
            stop_and_seek(l, (l.playhead + step).min(n - 1));
        }
        if i.key_pressed(Key::I) {
            l.start = l.playhead.min(l.end.saturating_sub(1));
        }
        if i.key_pressed(Key::O) {
            l.end = l.playhead.max(l.start + 1).min(n - 1);
        }
        if i.key_pressed(Key::Home) {
            stop_and_seek(l, effective_start(&l.probe, l.start, mode));
        }
        if i.key_pressed(Key::End) {
            stop_and_seek(l, l.end);
        }
    });
}

/// Gets the frame under the playhead onto the screen: from the cache, from
/// the running decoder, or by restarting the decoder at that frame.
fn pump(ctx: &egui::Context, l: &mut Loaded) {
    let n = l.probe.frame_count();
    let fps = l.probe.fps.max(1.0);
    if let Some((t0, f0)) = l.playing {
        let target = f0 + (t0.elapsed().as_secs_f64() * fps) as usize;
        if target >= l.end {
            l.playhead = l.end;
            l.playing = None;
        } else {
            l.playhead = target;
        }
        ctx.request_repaint();
    }
    let want = l.playhead.min(n - 1);
    if l.shown == Some(want) {
        if l.playing.is_none() && l.decoder.is_some() && l.last_activity.elapsed() > DECODER_IDLE {
            l.decoder = None; // paused and idle: free the ffmpeg process
        }
        return;
    }
    l.last_activity = Instant::now();
    if let Some(i) = l.cache.iter().position(|f| f.index == want) {
        show(ctx, l, i);
        return;
    }
    // Keep streaming if the decoder is just behind the wanted frame; else seek.
    let streaming = l.decoder.is_some() && l.decoder_next <= want && want - l.decoder_next <= (fps * 1.5) as usize;
    if !streaming {
        // While scrubbing, don't restart ffmpeg on every mouse move: let a
        // fresh decoder deliver its first frame (or give it 120 ms) first.
        let in_flight = l.decoder.as_ref().is_some_and(|d| l.decoder_next == d.start)
            && l.decoder_started.elapsed() < Duration::from_millis(120);
        if !in_flight {
            l.decoder = None;
            match Decoder::start(&l.probe, want, l.size) {
                Ok(d) => {
                    l.decoder = Some(d);
                    l.decoder_next = want;
                    l.decoder_started = Instant::now();
                }
                Err(e) => log::error!("preview: {e:#}"),
            }
        }
    }
    if let Some(d) = l.decoder.as_mut() {
        for _ in 0..8 {
            let Some(f) = d.try_next() else { break };
            l.decoder_next = f.index + 1;
            let idx = f.index;
            l.cache.push_back(f);
            if l.cache.len() > CACHE_FRAMES {
                l.cache.pop_front();
            }
            if idx >= want {
                let i = l.cache.len() - 1;
                show(ctx, l, i);
                break;
            }
        }
    }
    if l.shown != Some(want) {
        ctx.request_repaint_after(Duration::from_millis(4));
    }
}

fn show(ctx: &egui::Context, l: &mut Loaded, cache_index: usize) {
    let f = &l.cache[cache_index];
    let image = egui::ColorImage::from_rgba_unmultiplied([f.width as usize, f.height as usize], &f.rgba);
    match &mut l.texture {
        Some(t) => t.set(image, TextureOptions::LINEAR),
        None => l.texture = Some(ctx.load_texture("preview", image, TextureOptions::LINEAR)),
    }
    l.shown = Some(f.index);
}

/// The timeline: track, keyframe ticks, kept region, two draggable handles
/// and the playhead. Click or drag anywhere else to scrub.
fn timeline(ui: &mut egui::Ui, l: &mut Loaded, colors: &crate::themes::Colors, mode: TrimMode, radius: f32) {
    let n = l.probe.frame_count();
    let last = (n - 1).max(1) as f32;
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 62.0), Sense::click_and_drag());
    let track = Rect::from_min_max(pos2(rect.left() + 10.0, rect.top() + 16.0), pos2(rect.right() - 10.0, rect.bottom() - 10.0));
    let x_of = |f: usize| track.left() + track.width() * (f as f32 / last);
    let f_of = |x: f32| (((x - track.left()) / track.width()).clamp(0.0, 1.0) * last).round() as usize;
    let painter = ui.painter_at(rect);
    let dim = colors.text_dim.0;

    painter.rect_filled(track, radius.min(8.0), colors.timeline.0);
    // Outline, so the track stays visible when it's the same color as the
    // background (OLED black).
    painter.rect_stroke(track, radius.min(8.0), Stroke::new(1.0, colors.border.0), egui::StrokeKind::Inside);
    // Second marks along the top, labelled every 5 s.
    let dur = l.probe.duration();
    let mut s = 0.0;
    while s <= dur {
        let x = x_of(l.probe.frame_at(s));
        let major = (s as u32).is_multiple_of(5);
        painter.line_segment([pos2(x, track.top() - if major { 6.0 } else { 3.0 }), pos2(x, track.top())], Stroke::new(1.0, dim));
        if major && s > 0.0 {
            painter.text(pos2(x, rect.top()), egui::Align2::CENTER_TOP, format!("{}s", s as u32), egui::FontId::proportional(10.0), dim);
        }
        s += 1.0;
    }
    // Keyframes as small ticks at the bottom of the track.
    for &k in &l.probe.keyframes {
        let x = x_of(k);
        painter.line_segment([pos2(x, track.bottom() - 5.0), pos2(x, track.bottom())], Stroke::new(1.0, dim));
    }

    let eff = effective_start(&l.probe, l.start, mode);
    let sel = Rect::from_min_max(pos2(x_of(eff), track.top()), pos2(x_of(l.end), track.bottom()));
    painter.rect_filled(sel, 0.0, colors.timeline_selection.0);
    if mode == TrimMode::Fast && eff != l.start {
        // Where the handle was dragged to, before snapping.
        let x = x_of(l.start);
        painter.line_segment([pos2(x, track.top()), pos2(x, track.bottom())], Stroke::new(1.0, colors.accent.0.gamma_multiply(0.5)));
    }
    for f in [eff, l.end] {
        let handle = Rect::from_center_size(pos2(x_of(f), track.center().y), vec2(8.0, track.height() + 8.0));
        painter.rect_filled(handle, 3.0, colors.accent.0);
        for dy in [-4.0, 0.0, 4.0] {
            let y = handle.center().y + dy;
            painter.line_segment([pos2(handle.left() + 2.0, y), pos2(handle.right() - 2.0, y)], Stroke::new(1.0, colors.accent_text.0));
        }
    }
    let px = x_of(l.playhead);
    painter.line_segment([pos2(px, track.top() - 4.0), pos2(px, rect.bottom())], Stroke::new(2.0, colors.playhead.0));
    painter.add(egui::Shape::convex_polygon(
        vec![pos2(px - 5.0, track.top() - 9.0), pos2(px + 5.0, track.top() - 9.0), pos2(px, track.top() - 3.0)],
        colors.playhead.0,
        Stroke::NONE,
    ));

    // ---- interaction ----
    let near = |p: Pos2, f: usize| (p.x - x_of(f)).abs() <= 8.0;
    if let Some(p) = resp.hover_pos() {
        if near(p, eff) || near(p, l.end) {
            ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
        }
    }
    if resp.drag_started() {
        if let Some(p) = resp.interact_pointer_pos() {
            l.drag = Some(if near(p, l.end) {
                Drag::End
            } else if near(p, eff) {
                Drag::Start
            } else {
                Drag::Playhead
            });
            l.playing = None;
        }
    }
    if let (Some(drag), Some(p)) = (l.drag, resp.interact_pointer_pos()) {
        let f = f_of(p.x);
        match drag {
            Drag::Start => {
                l.start = f.min(l.end.saturating_sub(1));
                l.playhead = effective_start(&l.probe, l.start, mode);
            }
            Drag::End => {
                l.end = f.max(l.start + 1).min(n - 1);
                l.playhead = l.end;
            }
            Drag::Playhead => l.playhead = f,
        }
    }
    if resp.drag_stopped() {
        l.drag = None;
    }
    if resp.clicked() {
        if let Some(p) = resp.interact_pointer_pos() {
            stop_and_seek(l, f_of(p.x));
        }
    }
    if let Some(p) = resp.hover_pos() {
        let f = f_of(p.x);
        resp.on_hover_text_at_pointer(fmt_time(l.probe.time_of(f)));
    }
}

