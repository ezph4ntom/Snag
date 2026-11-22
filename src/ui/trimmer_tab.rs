//! Clips tab: the clip editor. Preview, a timeline with draggable start/end
//! handles, frame stepping, and pieces: an edit starts as the whole clip you
//! opened; Split cuts the selected piece at the playhead, Remove drops a
//! piece (split twice and remove the middle to cut a part out), "Add clip"
//! puts another clip after the others, and the arrows change the order.
//! Play runs through the pieces in order. Saving writes one clip with every
//! piece, or each piece as its own file (fast/lossless or precise: see
//! `trimmer`).
//!
//! Keyboard: Space play/pause · ←/→ one frame · Shift+←/→ one second ·
//! I / O set start / end at the playhead · S split · Delete remove the
//! piece · Home / End jump to the piece's start / end.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, CursorIcon, Key, Pos2, Rect, RichText, Sense, Stroke, TextureHandle, TextureOptions, pos2, vec2};

use super::library_tab::LibraryTab;
use super::{Shared, dim};
use crate::config::Settings;
use crate::trimmer::audio::AudioPlayer;
use crate::trimmer::player::{Decoder, Frame, decode_size, preview_size};
use crate::trimmer::{ClipProbe, EditJob, Output, Piece, TrimJob, TrimMode, export, fast_join_problem, probe, trim};

/// Memory for recently decoded preview frames (kept for instant stepping
/// back): about 48 MB, i.e. ~6 frames at 1080p, ~24 at 960x540.
const CACHE_BYTES: usize = 48 * 1024 * 1024;

/// Smooth when the full-resolution frame is drawn smaller than it is
/// (mipmaps avoid the shimmer plain down-scaling gives).
const PREVIEW_FILTER: TextureOptions = TextureOptions::LINEAR.with_mipmap_mode(Some(egui::TextureFilter::Linear));

#[derive(Clone, Copy, PartialEq, Eq)]
enum Drag {
    Start,
    End,
    Playhead,
}

struct Loaded {
    /// The edit, in order. Never empty.
    pieces: Vec<Piece>,
    /// The selected piece: its clip is on the timeline and in the preview.
    sel: usize,
    /// The selected piece's clip.
    probe: Arc<ClipProbe>,
    /// Size the preview is decoded at (see `decode_size`).
    size: (u32, u32),
    /// The preview area, in physical pixels (what `size` is chosen for).
    area_px: (f32, f32),
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

impl Loaded {
    fn new(probe: Arc<ClipProbe>) -> Loaded {
        Loaded {
            pieces: vec![Piece::whole(probe.clone())],
            sel: 0,
            size: preview_size(probe.width, probe.height),
            area_px: (0.0, 0.0),
            probe,
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
        }
    }

    fn piece(&self) -> &Piece {
        &self.pieces[self.sel]
    }

    /// The decode size for this clip in the current preview area.
    fn fit_size(&self) -> (u32, u32) {
        let full = preview_size(self.probe.width, self.probe.height);
        let (w, h) = (full.0 as f32, full.1 as f32);
        let scale = (self.area_px.0 / w).min(self.area_px.1 / h);
        if !scale.is_finite() || scale <= 0.0 {
            return full; // not laid out yet
        }
        decode_size(full, (w * scale, h * scale))
    }

    /// Selects piece `i` and puts the playhead on its first frame. Another
    /// clip's piece swaps the preview over to that clip.
    fn select(&mut self, i: usize, mode: TrimMode) {
        self.sel = i.min(self.pieces.len() - 1);
        let probe = self.pieces[self.sel].probe.clone();
        if !Arc::ptr_eq(&probe, &self.probe) {
            self.probe = probe;
            self.size = self.fit_size();
            self.decoder = None;
            self.cache.clear();
            self.shown = None;
            self.audio = None;
        }
        self.playhead = self.pieces[self.sel].first(mode);
    }

    fn set_start(&mut self, frame: usize) {
        let p = &mut self.pieces[self.sel];
        p.start = frame.min(p.end.saturating_sub(1));
    }

    fn set_end(&mut self, frame: usize) {
        let n = self.probe.frame_count();
        let p = &mut self.pieces[self.sel];
        p.end = frame.max(p.start + 1).min(n - 1);
    }

    /// Where Split would cut the selected piece: at the playhead or, in
    /// Fast mode, at the keyframe at or before it (a lossless piece can only
    /// start on a keyframe, so cutting anywhere else would repeat the frames
    /// in between). None if that isn't inside the piece.
    fn split_point(&self, mode: TrimMode) -> Option<usize> {
        let at = match mode {
            TrimMode::Fast => self.probe.keyframe_at_or_before(self.playhead),
            TrimMode::Precise => self.playhead,
        };
        self.piece().split_at(at).map(|_| at)
    }

    /// Splits the selected piece (see `split_point`); the second half
    /// becomes the selected piece. False if there's nowhere to split.
    fn split(&mut self, mode: TrimMode) -> bool {
        let Some(at) = self.split_point(mode) else { return false };
        let Some((a, b)) = self.piece().split_at(at) else { return false };
        self.pieces[self.sel] = a;
        self.pieces.insert(self.sel + 1, b);
        self.sel += 1;
        self.playing = None;
        self.playhead = at;
        true
    }

    fn remove(&mut self, mode: TrimMode) {
        if self.pieces.len() > 1 {
            self.playing = None;
            self.pieces.remove(self.sel);
            self.select(self.sel.min(self.pieces.len() - 1), mode);
        }
    }

    /// Moves the selected piece one place earlier (-1) or later (+1).
    fn move_by(&mut self, by: isize) {
        let to = self.sel as isize + by;
        if to >= 0 && (to as usize) < self.pieces.len() {
            self.pieces.swap(self.sel, to as usize);
            self.sel = to as usize;
        }
    }

    /// The clips in the edit, each once.
    fn paths(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        for p in &self.pieces {
            if !out.contains(&p.probe.path) {
                out.push(p.probe.path.clone());
            }
        }
        out
    }
}

const DECODER_IDLE: Duration = Duration::from_secs(3);

enum ExportMsg {
    Progress(f32),
    Done(Result<Vec<PathBuf>, String>),
}

struct Export {
    /// The clips being read.
    sources: Vec<PathBuf>,
    rx: Receiver<ExportMsg>,
    progress: f32,
    cancel: Arc<AtomicBool>,
    overwrite: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Save {
    /// One piece of one clip, replacing that clip.
    Overwrite,
    /// One clip with every piece (or `_trim` for a single piece).
    Joined,
    /// Each piece as its own file.
    Separate,
}

/// The "Add clip" list.
#[derive(Default)]
struct Picker {
    query: String,
    browse: Option<Receiver<PathBuf>>,
}

type Probing = (PathBuf, Receiver<Result<ClipProbe, String>>);

pub struct TrimmerTab {
    loaded: Option<Loaded>,
    loading: Option<Probing>,
    /// A clip being added to the edit (probed on a worker thread).
    adding: Option<Probing>,
    picker: Option<Picker>,
    precise: bool,
    export: Option<Export>,
    message: Option<(String, bool, Option<PathBuf>)>,
    confirm_overwrite: bool,
    /// Height the controls under the preview took last frame.
    controls_h: f32,
}

fn fmt_time(t: f64) -> String {
    let t = t.max(0.0);
    let m = (t / 60.0).floor();
    format!("{:02}:{:06.3}", m as u32, t - m * 60.0)
}

/// "1:05.3", a moment in a clip.
fn fmt_clock(t: f64) -> String {
    let t = (t.max(0.0) * 10.0).round() / 10.0;
    let m = (t / 60.0).floor();
    format!("{}:{:04.1}", m as u32, t - m * 60.0)
}

/// "1:05.3" (or "12.0 s" under a minute), for lengths.
fn fmt_length(t: f64) -> String {
    let t = (t.max(0.0) * 10.0).round() / 10.0;
    if t < 60.0 { format!("{t:.1} s") } else { fmt_clock(t) }
}

fn probe_in_background(path: PathBuf) -> Probing {
    let (tx, rx) = channel();
    let p = path.clone();
    std::thread::spawn(move || {
        let _ = tx.send(probe(&p).map_err(|e| format!("{e:#}")));
    });
    (path, rx)
}

impl TrimmerTab {
    pub fn new(settings: &Settings) -> Self {
        TrimmerTab {
            loaded: None,
            loading: None,
            adding: None,
            picker: None,
            precise: settings.trimmer.precise_by_default,
            export: None,
            message: None,
            confirm_overwrite: false,
            controls_h: 300.0,
        }
    }

    fn mode(&self) -> TrimMode {
        if self.precise { TrimMode::Precise } else { TrimMode::Fast }
    }

    /// Starts loading a clip (ffprobe runs on a worker thread). Replaces the
    /// edit that's open.
    pub fn load(&mut self, path: PathBuf) {
        self.loading = Some(probe_in_background(path));
    }

    /// Puts a clip after the pieces of the edit (the whole clip).
    fn add_clip(&mut self, path: PathBuf) {
        let mode = self.mode();
        let Some(l) = self.loaded.as_mut() else { return };
        // Already in the edit: no need to read it again.
        if let Some(p) = l.pieces.iter().find(|p| p.probe.path == path) {
            let piece = Piece::whole(p.probe.clone());
            l.playing = None;
            l.pieces.push(piece);
            l.select(l.pieces.len() - 1, mode);
            return;
        }
        self.adding = Some(probe_in_background(path));
    }

    /// Work that must happen even while the window is minimized.
    pub fn poll_background(&mut self, _settings: &Settings) {
        if let Some((path, rx)) = &self.loading {
            if let Ok(result) = rx.try_recv() {
                match result {
                    Ok(p) if p.frame_count() > 0 => {
                        self.loaded = Some(Loaded::new(Arc::new(p)));
                        self.message = None;
                    }
                    Ok(_) => self.message = Some((format!("{} has no frames", path.display()), true, None)),
                    Err(e) => self.message = Some((format!("Can't open {}: {e}", path.display()), true, None)),
                }
                self.loading = None;
            }
        }
        if let Some((path, rx)) = &self.adding {
            if let Ok(result) = rx.try_recv() {
                let mode = self.mode();
                match (result, self.loaded.as_mut()) {
                    (Ok(p), Some(l)) if p.frame_count() > 0 => {
                        l.playing = None;
                        l.pieces.push(Piece::whole(Arc::new(p)));
                        l.select(l.pieces.len() - 1, mode);
                        self.message = None;
                    }
                    (Ok(_), _) => self.message = Some((format!("{} has no frames", path.display()), true, None)),
                    (Err(e), _) => self.message = Some((format!("Can't add {}: {e}", path.display()), true, None)),
                }
                self.adding = None;
            }
        }
        if let Some(l) = self.loaded.as_mut() {
            if l.playing.is_none() && l.decoder.is_some() && l.last_activity.elapsed() > DECODER_IDLE {
                l.decoder = None;
                l.cache.clear();
                l.cache.shrink_to_fit();
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
                    Ok(paths) => {
                        let first = paths.first().cloned();
                        let text = match paths.as_slice() {
                            [one] => format!("Saved {}", one.file_name().unwrap_or_default().to_string_lossy()),
                            many => format!("Saved {} files", many.len()),
                        };
                        self.message = Some((text, false, first.clone()));
                        if let (true, Some(path)) = (overwrite && self.loaded.is_some(), first) {
                            self.load(path);
                        }
                    }
                    Err(e) => self.message = Some((format!("Export failed: {e}"), true, None)),
                }
            }
        }
    }

    /// The "Add clip" list is open (it shows the library's clips).
    pub fn wants_clip_list(&self) -> bool {
        self.picker.is_some()
    }

    /// A clip is open (or opening) in the editor.
    pub fn is_open(&self) -> bool {
        self.loaded.is_some() || self.loading.is_some()
    }

    /// Files the editor is using (open, opening, being added or exported).
    pub fn files_in_use(&self) -> Vec<PathBuf> {
        let mut out = self.loaded.as_ref().map(|l| l.paths()).unwrap_or_default();
        out.extend(self.loading.as_ref().map(|(p, _)| p.clone()));
        out.extend(self.adding.as_ref().map(|(p, _)| p.clone()));
        if let Some(e) = &self.export {
            out.extend(e.sources.iter().cloned());
        }
        out
    }

    /// Back to the library: closes the edit (and its decoder).
    pub fn close(&mut self) {
        self.loaded = None;
        self.loading = None;
        self.adding = None;
        self.picker = None;
        self.message = None;
    }

    /// The editor; returns true when "back to the library" was pressed.
    pub fn ui(&mut self, ui: &mut egui::Ui, shared: &mut Shared, library: &LibraryTab) -> bool {
        if self.loading.is_some() || self.adding.is_some() || self.export.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(30));
        }
        let mut back = false;
        ui.horizontal(|ui| {
            let b = egui::Button::new("\u{2039}  Library").corner_radius(egui::CornerRadius::same(14)).min_size(vec2(0.0, 28.0));
            if ui.add(b).on_hover_text("Back to all clips").clicked() {
                back = true;
            }
            let name_of = |p: &PathBuf| p.file_name().unwrap_or_default().to_string_lossy().to_string();
            let title = match (&self.loaded, &self.loading) {
                (Some(l), _) => {
                    let paths = l.paths();
                    match paths.len() {
                        1 => name_of(&paths[0]),
                        2 => format!("{} + 1 more clip", name_of(&paths[0])),
                        n => format!("{} + {} more clips", name_of(&paths[0]), n - 1),
                    }
                }
                (None, Some((p, _))) => name_of(p),
                _ => String::new(),
            };
            if !title.is_empty() {
                ui.add_space(6.0);
                ui.add(egui::Label::new(RichText::new(title).strong()).truncate());
            }
        });
        ui.add_space(8.0);
        if self.loaded.is_some() {
            self.editor(ui, shared);
        } else if self.loading.is_some() {
            ui.centered_and_justified(|ui| ui.spinner());
        } else if let Some((msg, _, _)) = &self.message {
            ui.label(RichText::new(msg).color(shared.theme.colors.error.0));
        }
        self.overwrite_dialog(ui.ctx(), shared);
        self.picker_dialog(ui.ctx(), shared, library);
        back
    }

    fn editor(&mut self, ui: &mut egui::Ui, shared: &mut Shared) {
        let mode = self.mode();
        let exporting_overwrite = self.export.as_ref().is_some_and(|e| e.overwrite);
        let busy = self.export.is_some();
        let ctx = ui.ctx().clone();
        let typing = ctx.memory(|m| m.focused().is_some());
        let dialog_open = self.picker.is_some() || self.confirm_overwrite;
        let colors = shared.theme.colors.clone();
        let adding = self.adding.is_some();
        let mut open_picker = false;
        let l = self.loaded.as_mut().expect("checked by caller");

        if !typing && !dialog_open {
            handle_keys(&ctx, l, mode);
        }
        // The preview area: the controls below take what they took last
        // frame (rows wrap on narrow windows); the preview gets the rest.
        // (At the smallest window size the controls need nearly all of it.)
        let avail = ui.available_size();
        let area = vec2(avail.x, (avail.y - self.controls_h).max(80.0));
        let ppp = ctx.pixels_per_point();
        l.area_px = (area.x * ppp, area.y * ppp);
        let fit = l.fit_size();
        if fit != l.size {
            // Window resized past a step: decode at the new size.
            l.size = fit;
            l.decoder = None;
            l.cache.clear();
            l.shown = None;
        }
        if !exporting_overwrite {
            pump(&ctx, l, mode);
        }
        sync_audio(l, shared.settings.trimmer.preview_volume);
        let several = l.pieces.len() > 1;

        // ---- preview -------------------------------------------------------
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
        let controls_top = ui.cursor().top();
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            if ui.button("|<").on_hover_text("Go to the piece's start (Home)").clicked() {
                let f = l.piece().first(mode);
                stop_and_seek(l, f);
            }
            if ui.button("<").on_hover_text("Previous frame (Left arrow; Shift: 1 s)").clicked() {
                stop_and_seek(l, l.playhead.saturating_sub(1));
            }
            let play_label = if l.playing.is_some() { "Pause" } else { "Play" };
            let play_tip = if several { "Space. Plays through the pieces in order." } else { "Space" };
            if ui.add(egui::Button::new(play_label).min_size(vec2(80.0, 0.0))).on_hover_text(play_tip).clicked() {
                toggle_play(l, mode);
            }
            if ui.button(">").on_hover_text("Next frame (Right arrow; Shift: 1 s)").clicked() {
                stop_and_seek(l, (l.playhead + 1).min(n - 1));
            }
            if ui.button(">|").on_hover_text("Go to the piece's end (End)").clicked() {
                let f = l.piece().end;
                stop_and_seek(l, f);
            }
            ui.separator();
            if ui.button("Start [I]").on_hover_text("Start the piece at the playhead").clicked() {
                l.set_start(l.playhead);
            }
            if ui.button("End [O]").on_hover_text("End the piece at the playhead").clicked() {
                l.set_end(l.playhead);
            }
            let split_tip = match mode {
                TrimMode::Fast => "Cut the piece in two at the keyframe before the playhead (Fast mode cuts on keyframes, one per second). To remove a part, split before and after it.",
                TrimMode::Precise => "Cut the piece in two at the playhead. To remove a part, split before and after it.",
            };
            let split = ui
                .add_enabled(l.split_point(mode).is_some(), egui::Button::new("Split [S]"))
                .on_hover_text(split_tip)
                .on_disabled_hover_text("Move the playhead inside the piece (past its first second in Fast mode) to split it there");
            if split.clicked() {
                l.split(mode);
            }
            ui.separator();
            // The volume slider is a few widgets side by side: start a new
            // row rather than let it run past the window's edge.
            if p.audio_tracks > 0 && ui.available_size_before_wrap().x < 200.0 {
                ui.end_row();
            }
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
            // Never split mid-text; on a narrow window it moves down whole
            // (without a divider at the start of the new row).
            let time = RichText::new(format!("{} / {}", fmt_time(p.time_of(l.playhead)), fmt_time(p.duration()))).monospace();
            let time_w = super::widgets::text_width(ui, time.clone(), egui::TextStyle::Monospace);
            if ui.available_size_before_wrap().x < time_w + 16.0 {
                ui.end_row();
            } else {
                ui.separator();
            }
            ui.add(egui::Label::new(time).wrap_mode(egui::TextWrapMode::Extend));
        });

        // ---- timeline ----------------------------------------------------------
        ui.add_space(4.0);
        timeline(ui, l, &colors, mode, shared.theme.corner_radius);

        // ---- pieces ------------------------------------------------------------
        ui.add_space(6.0);
        if pieces_strip(ui, l, &colors, mode, !adding && !busy) {
            open_picker = true;
        }

        // ---- selection info + export -----------------------------------------
        let piece = l.piece().clone();
        let eff = piece.first(mode);
        let join_problem = if several && mode == TrimMode::Fast { fast_join_problem(&l.pieces) } else { None };
        ui.horizontal_wrapped(|ui| {
            if several {
                if ui.add_enabled(l.sel > 0, egui::Button::new("Earlier").small()).on_hover_text("Move the piece one place earlier").clicked() {
                    l.move_by(-1);
                }
                if ui.add_enabled(l.sel + 1 < l.pieces.len(), egui::Button::new("Later").small()).on_hover_text("Move the piece one place later").clicked() {
                    l.move_by(1);
                }
                if ui.add(egui::Button::new("Remove").small()).on_hover_text("Leave this piece out (Delete)").clicked() {
                    l.remove(mode);
                }
                ui.label(RichText::new(format!("Piece {} of {}:", l.sel + 1, l.pieces.len())).strong());
            }
            ui.label(format!(
                "Start {}   End {}   Length {}",
                fmt_time(p.time_of(eff)),
                fmt_time(p.time_of(piece.end) + p.frame_duration()),
                fmt_time(piece.seconds(mode))
            ));
            if mode == TrimMode::Fast && eff != piece.start {
                ui.label(dim(shared, "(start snapped back to a keyframe)"));
            }
            if several {
                let total: f64 = l.pieces.iter().map(|p| p.seconds(mode)).sum();
                ui.label(RichText::new(format!("Total {}", fmt_length(total))).strong());
            }
        });
        ui.add_space(4.0);
        let mut save = None;
        ui.horizontal_wrapped(|ui| {
            ui.label("Mode:");
            ui.radio_value(&mut self.precise, false, "Fast")
                .on_hover_text("Lossless and instant. Starts snap to the previous keyframe (one per second).");
            ui.radio_value(&mut self.precise, true, "Precise")
                .on_hover_text("Frame-exact starts, and joins any clips. Re-encodes with your encoder at high bitrate (takes a few seconds).");
            ui.separator();
            let accent = |text: &str| egui::Button::new(RichText::new(text).color(colors.accent_text.0)).fill(colors.accent.0);
            if several {
                let one = ui
                    .add_enabled(!busy && join_problem.is_none(), accent("Save as one clip"))
                    .on_hover_text("Every piece in order, in a new file (the originals stay)");
                if one.clicked() {
                    save = Some(Save::Joined);
                }
                if ui.add_enabled(!busy, egui::Button::new("Save each piece")).on_hover_text("One new file per piece").clicked() {
                    save = Some(Save::Separate);
                }
            } else {
                if ui.add_enabled(!busy, egui::Button::new("Save as new file")).clicked() {
                    save = Some(Save::Joined);
                }
                if ui.add_enabled(!busy, accent("Overwrite original")).clicked() {
                    self.confirm_overwrite = true;
                }
            }
            if ui.button("Show in folder").clicked() {
                crate::win::reveal_in_explorer(&p.path);
            }
        });
        // One status line, always there (so the preview doesn't jump when
        // something shows up in it): export progress, the last result, why
        // the clips can't be joined losslessly, or a clip being added.
        let mut reveal = None;
        ui.allocate_ui_with_layout(vec2(ui.available_width(), 26.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            if let Some(ex) = &self.export {
                ui.add(egui::ProgressBar::new(ex.progress).desired_width(320.0).show_percentage());
                if ui.button("Cancel").clicked() {
                    ex.cancel.store(true, Ordering::Relaxed);
                }
            } else if let Some((msg, is_err, path)) = &self.message {
                let col = if *is_err { colors.error.0 } else { colors.success.0 };
                ui.add(egui::Label::new(RichText::new(msg).color(col)).truncate());
                if let Some(p) = path {
                    if ui.small_button("Show").clicked() {
                        reveal = Some(p.clone());
                    }
                }
            } else if let Some(why) = join_problem {
                let text = format!("{why}, so they can only be joined in Precise mode.");
                ui.add(egui::Label::new(RichText::new(text).color(colors.warning.0)).truncate());
            } else if adding {
                ui.spinner();
                ui.label(dim(shared, "Adding clip..."));
            }
        });
        if let Some(p) = reveal {
            crate::win::reveal_in_explorer(&p);
        }
        let used = ui.cursor().top() - controls_top;
        if (used - self.controls_h).abs() > 0.5 {
            self.controls_h = used;
            ctx.request_repaint();
        }
        if open_picker {
            self.picker = Some(Picker::default());
        }
        if let Some(s) = save {
            self.start_export(s, shared);
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
                self.start_export(Save::Overwrite, shared);
            }
        }
    }

    /// "Add clip": the library's clips (newest first), or any video.
    fn picker_dialog(&mut self, ctx: &egui::Context, shared: &Shared, library: &LibraryTab) {
        let Some(picker) = self.picker.as_mut() else { return };
        if let Some(path) = picker.browse.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.picker = None;
            self.add_clip(path);
            return;
        }
        if picker.browse.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        let theme = shared.theme.clone();
        let c = &theme.colors;
        let mut chosen = None;
        let mut close = false;
        let screen = ctx.content_rect();
        let resp = egui::Modal::new(egui::Id::unique("add-clip")).show(ctx, |ui| {
            ui.set_width(560.0_f32.min(screen.width() - 80.0));
            ui.label(RichText::new("Add a clip").family(crate::themes::brand_bold_family()).size(theme.fonts.body + 5.0));
            ui.label(dim(shared, "It goes after the pieces you have. Clips of the same size and frame rate join losslessly."));
            ui.add_space(8.0);
            super::widgets::search_box(ui, &theme, &mut picker.query, ui.available_width().min(280.0), false, "Search clips");
            ui.add_space(6.0);
            let q = picker.query.trim().to_lowercase();
            let clips: Vec<_> = library
                .clip_choices()
                .into_iter()
                .filter(|ch| q.is_empty() || ch.name.to_lowercase().contains(&q) || ch.detail.to_lowercase().contains(&q))
                .collect();
            let row_h = 52.0;
            let list_h = (screen.height() - 260.0).clamp(120.0, 420.0);
            if clips.is_empty() {
                ui.add_space(8.0);
                ui.label(dim(shared, if q.is_empty() { "No clips in the library yet." } else { "No clips match." }));
                ui.add_space(8.0);
            } else {
                egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, true]).show_rows(ui, row_h, clips.len(), |ui, range| {
                    for ch in &clips[range] {
                        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
                        resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, &ch.name));
                        let p = ui.painter();
                        if resp.hovered() {
                            p.rect_filled(rect, 8.0, c.widget_hover.0);
                        }
                        let thumb = Rect::from_min_size(rect.min + vec2(6.0, 5.0), vec2(74.0, 42.0));
                        p.rect_filled(thumb, 5.0, c.preview.0);
                        if let Some(t) = &ch.thumb {
                            p.add(
                                egui::epaint::RectShape::filled(thumb, 5.0, Color32::WHITE)
                                    .with_texture(t.id(), Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0))),
                            );
                        }
                        let x = thumb.right() + 12.0;
                        let w = (rect.right() - x - 8.0).max(10.0);
                        let name = super::widgets::one_line(p, ch.name.clone(), egui::FontId::proportional(theme.fonts.body), c.text.0, w);
                        let detail = super::widgets::one_line(p, ch.detail.clone(), egui::FontId::proportional(theme.fonts.body - 2.0), c.text_dim.0, w);
                        p.with_clip_rect(rect).galley(pos2(x, rect.top() + 8.0), name, c.text.0);
                        p.with_clip_rect(rect).galley(pos2(x, rect.top() + 28.0), detail, c.text_dim.0);
                        if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                            chosen = Some(ch.path.clone());
                        }
                    }
                });
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(picker.browse.is_none(), egui::Button::new("Browse...")).on_hover_text("Any video file").clicked() {
                    picker.browse = Some(super::dialogs::pick_video(&shared.settings.clips_dir));
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if let Some(path) = chosen {
            self.picker = None;
            self.add_clip(path);
        } else if close || resp.should_close() {
            self.picker = None;
        }
    }

    fn start_export(&mut self, save: Save, shared: &Shared) {
        let mode = self.mode();
        let Some(l) = self.loaded.as_mut() else { return };
        l.playing = None;
        let overwrite = save == Save::Overwrite;
        if overwrite {
            // ffmpeg's preview decoder holds the file open; Windows won't
            // replace a file that's open, so release it first.
            l.decoder = None;
        }
        let encoder = shared.settings.video.encoder;
        let pieces = l.pieces.clone();
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        std::thread::spawn(move || {
            let ptx = tx.clone();
            let progress = move |p| {
                let _ = ptx.send(ExportMsg::Progress(p));
            };
            let result = if overwrite {
                let p = &pieces[0];
                let job = TrimJob { start: p.start, end: p.end, mode, output: Output::Overwrite, encoder };
                trim(&p.probe, &job, &progress, &c).map(|p| vec![p])
            } else {
                let job = EditJob { pieces, mode, separate: save == Save::Separate, encoder };
                export(&job, &progress, &c)
            };
            let _ = tx.send(ExportMsg::Done(result.map_err(|e| format!("{e:#}"))));
        });
        self.message = None;
        self.export = Some(Export { sources: l.paths(), rx, progress: 0.0, cancel, overwrite });
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
        // At (or past) the piece's end: on to the next piece, or back to
        // the first one after the last.
        if l.playhead >= l.piece().end {
            let next = if l.sel + 1 < l.pieces.len() { l.sel + 1 } else { 0 };
            l.select(next, mode);
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
            l.set_start(l.playhead);
        }
        if i.key_pressed(Key::O) {
            l.set_end(l.playhead);
        }
        if i.key_pressed(Key::S) {
            l.split(mode);
        }
        if i.key_pressed(Key::Delete) {
            l.remove(mode);
        }
        if i.key_pressed(Key::Home) {
            let f = l.piece().first(mode);
            stop_and_seek(l, f);
        }
        if i.key_pressed(Key::End) {
            let f = l.piece().end;
            stop_and_seek(l, f);
        }
    });
}

/// Gets the frame under the playhead onto the screen: from the cache, from
/// the running decoder, or by restarting the decoder at that frame. While
/// playing, the end of a piece moves on to the next one.
fn pump(ctx: &egui::Context, l: &mut Loaded, mode: TrimMode) {
    let fps = l.probe.fps.max(1.0);
    if let Some((t0, f0)) = l.playing {
        let target = f0 + (t0.elapsed().as_secs_f64() * fps) as usize;
        let end = l.piece().end;
        if target >= end && l.playhead <= end {
            if l.sel + 1 < l.pieces.len() {
                l.select(l.sel + 1, mode);
                l.playing = Some((Instant::now(), l.playhead));
            } else {
                l.playhead = end;
                l.playing = None;
            }
        } else {
            l.playhead = target;
        }
        // Draw again when the next frame is due, not at the screen's rate
        // (a 144 or 240 Hz monitor would draw each frame of a 60 fps clip
        // several times).
        if let Some((t0, f0)) = l.playing {
            let due = (l.playhead.saturating_sub(f0) + 1) as f64 / fps;
            ctx.request_repaint_after(Duration::from_secs_f64((due - t0.elapsed().as_secs_f64()).max(0.0)));
        }
    }
    let n = l.probe.frame_count();
    let fps = l.probe.fps.max(1.0);
    let want = l.playhead.min(n - 1);
    if l.shown == Some(want) {
        if l.playing.is_none() && l.decoder.is_some() && l.last_activity.elapsed() > DECODER_IDLE {
            l.decoder = None; // paused and idle: free the ffmpeg process
            // ...and the cached frames (the one on screen stays as a texture).
            l.cache.clear();
            l.cache.shrink_to_fit();
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
            let frame_bytes = (l.size.0 * l.size.1 * 4) as usize;
            if l.cache.len() > (CACHE_BYTES / frame_bytes.max(1)).max(4) {
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
    // Video frames are opaque, so "premultiplied" is the same picture
    // without egui checking every pixel's alpha.
    let image = egui::ColorImage::from_rgba_premultiplied([f.width as usize, f.height as usize], &f.rgba);
    match &mut l.texture {
        Some(t) => t.set(image, PREVIEW_FILTER),
        None => l.texture = Some(ctx.load_texture("preview", image, PREVIEW_FILTER)),
    }
    l.shown = Some(f.index);
}

/// The pieces in order, as cards (click one to select it), then an "Add
/// clip" card. Returns true when that was clicked.
fn pieces_strip(ui: &mut egui::Ui, l: &mut Loaded, colors: &crate::themes::Colors, mode: TrimMode, can_add: bool) -> bool {
    let card = vec2(168.0, 50.0);
    let mut clicked = None;
    let mut add = false;
    egui::ScrollArea::horizontal().id_salt("pieces").max_height(card.y + 6.0).show(ui, |ui| {
        ui.horizontal(|ui| {
            for (i, piece) in l.pieces.iter().enumerate() {
                let (rect, resp) = ui.allocate_exact_size(card, Sense::click());
                resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, i == l.sel, format!("Piece {}", i + 1)));
                let selected = i == l.sel;
                let p = ui.painter_at(rect.expand(1.0));
                let bg = if selected {
                    colors.timeline_selection.0
                } else if resp.hovered() {
                    colors.widget_active.0
                } else {
                    colors.widget_hover.0
                };
                p.rect_filled(rect, 8.0, bg);
                if selected {
                    p.rect_stroke(rect, 8.0, Stroke::new(2.0, colors.accent.0), egui::StrokeKind::Inside);
                }
                let probe = &piece.probe;
                let name = probe.path.file_stem().unwrap_or_default().to_string_lossy().to_string();
                let text_w = rect.width() - 16.0;
                let head = super::widgets::one_line(&p, format!("{}  {name}", i + 1), egui::FontId::proportional(12.5), colors.text.0, text_w);
                let first = piece.first(mode);
                let sub = format!(
                    "{}\u{2013}{} · {}",
                    fmt_clock(probe.time_of(first)),
                    fmt_clock(probe.time_of(piece.end) + probe.frame_duration()),
                    fmt_length(piece.seconds(mode))
                );
                let sub = super::widgets::one_line(&p, sub, egui::FontId::proportional(11.0), colors.text_dim.0, text_w);
                let clip = rect.shrink2(vec2(8.0, 0.0));
                p.with_clip_rect(clip).galley(rect.min + vec2(8.0, 8.0), head, colors.text.0);
                p.with_clip_rect(clip).galley(rect.min + vec2(8.0, 28.0), sub, colors.text_dim.0);
                if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                    clicked = Some(i);
                }
                ui.add_space(4.0);
            }
            // "+ Add clip": a dashed card after the pieces.
            let (rect, resp) = ui.allocate_exact_size(vec2(120.0, card.y), if can_add { Sense::click() } else { Sense::hover() });
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, can_add, "Add clip..."));
            let p = ui.painter_at(rect.expand(1.0));
            if can_add && resp.hovered() {
                p.rect_filled(rect, 8.0, colors.widget_hover.0);
            }
            let fg = if can_add { colors.text.0 } else { colors.text_dim.0 };
            let dashes = egui::Shape::dashed_line(
                &[rect.left_top(), rect.right_top(), rect.right_bottom(), rect.left_bottom(), rect.left_top()].map(|q| q + vec2(0.5, 0.5)),
                Stroke::new(1.0, colors.border.0),
                5.0,
                4.0,
            );
            p.extend(dashes);
            p.text(rect.center(), egui::Align2::CENTER_CENTER, "+  Add clip", egui::FontId::proportional(12.5), fg);
            let resp = resp.on_hover_text("Put another clip after the pieces, to join them");
            if can_add && resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                add = true;
            }
        });
    });
    if let Some(i) = clicked {
        l.playing = None;
        l.select(i, mode);
    }
    add
}

/// The timeline: track, keyframe ticks, the pieces of this clip (the
/// selected one with two draggable handles) and the playhead. Click or drag
/// anywhere else to scrub; a click inside another piece selects it.
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

    // The other pieces of this clip, dimmer, numbered when there are several.
    let several = l.pieces.len() > 1;
    for (i, piece) in l.pieces.iter().enumerate() {
        if i == l.sel || !Arc::ptr_eq(&piece.probe, &l.probe) {
            continue;
        }
        let r = Rect::from_min_max(pos2(x_of(piece.first(mode)), track.top()), pos2(x_of(piece.end), track.bottom()));
        painter.rect_filled(r, 0.0, colors.timeline_selection.0.gamma_multiply(0.45));
        painter.text(r.left_top() + vec2(4.0, 2.0), egui::Align2::LEFT_TOP, (i + 1).to_string(), egui::FontId::proportional(10.0), dim);
    }
    let piece = l.piece().clone();
    let eff = piece.first(mode);
    let sel = Rect::from_min_max(pos2(x_of(eff), track.top()), pos2(x_of(piece.end), track.bottom()));
    painter.rect_filled(sel, 0.0, colors.timeline_selection.0);
    if several {
        painter.text(sel.left_top() + vec2(7.0, 2.0), egui::Align2::LEFT_TOP, (l.sel + 1).to_string(), egui::FontId::proportional(10.0), colors.text.0);
    }
    if mode == TrimMode::Fast && eff != piece.start {
        // Where the handle was dragged to, before snapping.
        let x = x_of(piece.start);
        painter.line_segment([pos2(x, track.top()), pos2(x, track.bottom())], Stroke::new(1.0, colors.accent.0.gamma_multiply(0.5)));
    }
    for f in [eff, piece.end] {
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
        if near(p, eff) || near(p, piece.end) {
            ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
        }
    }
    if resp.drag_started() {
        if let Some(p) = resp.interact_pointer_pos() {
            l.drag = Some(if near(p, piece.end) {
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
                l.set_start(f);
                l.playhead = l.piece().first(mode);
            }
            Drag::End => {
                l.set_end(f);
                l.playhead = l.piece().end;
            }
            Drag::Playhead => l.playhead = f,
        }
    }
    if resp.drag_stopped() {
        l.drag = None;
    }
    if resp.clicked() {
        if let Some(p) = resp.interact_pointer_pos() {
            let f = f_of(p.x);
            let in_selected = (eff..=piece.end).contains(&f);
            let other = l
                .pieces
                .iter()
                .enumerate()
                .find(|(i, q)| *i != l.sel && Arc::ptr_eq(&q.probe, &l.probe) && (q.first(mode)..=q.end).contains(&f))
                .map(|(i, _)| i);
            if let (false, Some(i)) = (in_selected, other) {
                l.sel = i; // same clip: no preview change needed
            }
            stop_and_seek(l, f);
        }
    }
    if let Some(p) = resp.hover_pos() {
        let f = f_of(p.x);
        resp.on_hover_text_at_pointer(fmt_time(l.probe.time_of(f)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(name: &str, frames: usize) -> Arc<ClipProbe> {
        Arc::new(ClipProbe {
            path: PathBuf::from(name),
            width: 1920,
            height: 1080,
            fps: 60.0,
            times: (0..frames).map(|i| i as f64 / 60.0).collect(),
            keyframes: (0..frames).step_by(60).collect(),
            bytes: 0,
            video_offset: 0.0,
            audio_tracks: 1,
            video_format: String::new(),
            audio_formats: Vec::new(),
            title: None,
        })
    }

    #[test]
    fn cut_out_the_middle_then_add_and_reorder_a_clip() {
        let mode = TrimMode::Precise;
        let mut l = Loaded::new(clip("a.mp4", 600));
        // Split at 2 s and at 4 s, remove the middle: 0-119 and 240-599 stay.
        l.playhead = 120;
        assert!(l.split(mode));
        l.playhead = 240;
        assert!(l.split(mode));
        assert_eq!(l.sel, 2);
        l.select(1, mode);
        l.remove(mode);
        let ranges: Vec<_> = l.pieces.iter().map(|p| (p.start, p.end)).collect();
        assert_eq!(ranges, [(0, 119), (240, 599)]);
        // The selection moved to the piece after the removed one.
        assert_eq!((l.sel, l.playhead), (1, 240));
        // Splitting outside the piece does nothing.
        l.playhead = 50;
        assert!(!l.split(mode));

        // Fast mode splits on the keyframe before the playhead (no frames
        // in both halves); not at all within a piece's first second.
        let mut f = Loaded::new(clip("c.mp4", 600));
        f.playhead = 150;
        assert_eq!(f.split_point(TrimMode::Fast), Some(120));
        assert!(f.split(TrimMode::Fast));
        assert_eq!((f.pieces[0].end, f.pieces[1].start, f.playhead), (119, 120, 120));
        f.playhead = 150;
        assert_eq!(f.split_point(TrimMode::Fast), None);
        assert_eq!(f.split_point(TrimMode::Precise), Some(150));

        // Another clip: selecting it switches the preview to it.
        l.pieces.push(Piece::whole(clip("b.mp4", 300)));
        l.select(2, mode);
        assert_eq!(l.probe.path, PathBuf::from("b.mp4"));
        assert_eq!(l.paths(), [PathBuf::from("a.mp4"), PathBuf::from("b.mp4")]);
        l.move_by(-1);
        l.move_by(-1);
        assert_eq!(l.sel, 0);
        assert_eq!(l.pieces[0].probe.path, PathBuf::from("b.mp4"));
        l.move_by(-1); // already first
        assert_eq!(l.sel, 0);
        // End handle can't go before the start or past the clip.
        l.set_end(10_000);
        assert_eq!(l.piece().end, 299);
        l.set_start(299);
        assert_eq!(l.piece().start, 298);
    }

    #[test]
    fn play_runs_through_the_pieces() {
        let mode = TrimMode::Precise;
        let mut l = Loaded::new(clip("a.mp4", 600));
        l.pieces = vec![
            Piece { probe: l.probe.clone(), start: 0, end: 59 },
            Piece { probe: l.probe.clone(), start: 300, end: 359 },
        ];
        l.select(0, mode);
        l.playhead = 59;
        // At the end of the first piece, Play goes on to the second...
        toggle_play(&mut l, mode);
        assert_eq!((l.sel, l.playhead), (1, 300));
        // ...and at the end of the last, back to the first.
        l.playing = None;
        l.playhead = 359;
        toggle_play(&mut l, mode);
        assert_eq!((l.sel, l.playhead), (0, 0));
        assert_eq!(fmt_length(65.34), "1:05.3");
        assert_eq!(fmt_length(12.0), "12.0 s");
        assert_eq!(fmt_clock(59.97), "1:00.0");
        assert_eq!(fmt_clock(2.8), "0:02.8");
    }
}
