//! Live tab: what's being recorded right now, and the main buttons.
//!
//! Left: status with the replay buffer's on/off switch, the live preview
//! (switchable off: then no preview frames are made at all), three tiles
//! (Save clip / Screenshot / Record) and the latest clips in a row (left
//! out when the window is too small for it and a decent preview).
//! Right: "Sources" (what is recorded: a monitor, or only one app) and
//! "Sound in clips" (quick volume sliders).
//!
//! The preview picture comes from the core's recorder (see
//! `capture::preview`), so it shows exactly the monitor being captured and
//! moves with it when the capture follows focus to another monitor.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant, SystemTime};

use eframe::egui::{self, Color32, CornerRadius, Rect, RichText, Sense, TextureOptions, pos2, vec2};

use super::widgets::{fmt_duration, status_pill};
use super::{Shared, dim};
use crate::app::ipc::UiToCore;
use crate::capture::window::AppWindow;
use crate::config::CaptureMode;

/// What the Live tab asks the window to do.
pub enum LiveAction {
    /// Open this clip in the trimmer.
    OpenClip(PathBuf),
    /// Show the Clips tab.
    ShowClips,
}

struct Recent {
    path: PathBuf,
    name: String,
}

/// A thumbnail and length, made by the worker thread.
struct Thumb {
    path: PathBuf,
    rgba: Option<(u32, u32, Vec<u8>)>,
    seconds: Option<f64>,
}

#[derive(Default)]
pub struct LiveTab {
    reader: crate::capture::preview::Reader,
    texture: Option<egui::TextureHandle>,
    size: (u32, u32),
    recent: Vec<Recent>,
    last_scan: Option<Instant>,
    /// path -> (texture, seconds); `None` texture while being made.
    thumbs: HashMap<PathBuf, (Option<egui::TextureHandle>, Option<f64>)>,
    worker: Option<(Sender<PathBuf>, Receiver<Thumb>)>,
    monitors: Option<Vec<crate::capture::MonitorInfo>>,
    /// The "Record one app" picker is open, with the windows listed when
    /// it opened (only listed then, and on Refresh).
    picker: Option<Vec<AppWindow>>,
}

impl LiveTab {
    pub fn ui(&mut self, ui: &mut egui::Ui, shared: &mut Shared, sound: &mut super::sound_tab::SoundTab) -> Option<LiveAction> {
        let mut action = None;
        let preview_on = shared.settings.ui.live_preview;
        if preview_on {
            if let Some((w, h, rgba)) = self.reader.next_frame() {
                // Opaque frames: "premultiplied" is the same picture without
                // a per-pixel alpha check.
                let image = egui::ColorImage::from_rgba_premultiplied([w as usize, h as usize], &rgba);
                match &mut self.texture {
                    Some(t) => t.set(image, TextureOptions::LINEAR),
                    None => self.texture = Some(ui.ctx().load_texture("live-preview", image, TextureOptions::LINEAR)),
                }
                self.size = (w, h);
            }
            // No timed redraw here: `preview::watch` wakes the window when
            // the core writes a new frame.
        } else if self.texture.is_some() {
            self.texture = None; // free the preview's memory while it's off
        }

        let avail = ui.available_size();
        let right_w = 320.0f32.min(avail.x * 0.36);
        let gap = 20.0;
        let left_w = (avail.x - right_w - gap).max(200.0);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            ui.allocate_ui_with_layout(vec2(left_w, avail.y), egui::Layout::top_down(egui::Align::Min), |ui| {
                action = self.main_column(ui, shared);
            });
            ui.allocate_ui_with_layout(vec2(right_w, avail.y), egui::Layout::top_down(egui::Align::Min), |ui| {
                egui::ScrollArea::vertical().id_salt("live-side").auto_shrink([false, false]).show(ui, |ui| {
                    self.sources(ui, shared);
                    ui.add_space(14.0);
                    card(ui, shared, "Sound in clips", |ui, shared| sound.compact(ui, shared));
                });
            });
        });
        self.picker_dialog(ui.ctx(), shared);
        action
    }

    /// What is recorded: follow focus, one monitor, or only one app.
    fn sources(&mut self, ui: &mut egui::Ui, shared: &mut Shared) {
        let monitors = self.monitors.get_or_insert_with(crate::capture::enumerate_monitors).clone();
        let mut cap = shared.settings.capture.clone();
        let mut open_picker = false;
        let status = shared.status.clone().filter(|s| shared.connected && s.recording);
        card(ui, shared, "Sources", |ui, shared| {
            let c = shared.theme.colors.clone();
            if ui.radio(cap.mode == CaptureMode::FollowFocus, "Screen: follow focus").on_hover_text("The monitor with the active window").clicked() {
                cap.mode = CaptureMode::FollowFocus;
            }
            for m in &monitors {
                let on = cap.mode == CaptureMode::Fixed
                    && (cap.fixed_monitor == m.device_name || (cap.fixed_monitor.is_empty() && m.primary));
                let name = format!("{} {}x{}{}", m.device_name.trim_start_matches(r"\\.\"), m.width, m.height, if m.primary { " (primary)" } else { "" });
                if ui.radio(on, name).on_hover_text("Always this monitor").clicked() {
                    cap.mode = CaptureMode::Fixed;
                    cap.fixed_monitor = m.device_name.clone();
                }
            }
            let app_on = cap.mode == CaptureMode::Window && !cap.window_exe.is_empty();
            ui.horizontal(|ui| {
                let has_app = !cap.window_exe.is_empty();
                let label = if has_app { format!("Only {}", cap.window_exe) } else { "App or game...".to_string() };
                // "Choose..." keeps its room; a long program name is cut
                // with "…" instead of widening the whole column.
                let choose_w = if has_app { super::widgets::button_width(ui, "Choose...") + ui.spacing().item_spacing.x } else { 0.0 };
                let room = (ui.available_width() - choose_w).max(60.0);
                let row_h = ui.spacing().interact_size.y;
                let r = ui
                    .allocate_ui_with_layout(vec2(room, row_h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                        ui.radio(app_on, label)
                    })
                    .inner;
                let tip = "Record one app's window: other windows never show up, even after Alt-Tab";
                let tip = if has_app { format!("{}\n{tip}", cap.window_exe) } else { tip.to_string() };
                if r.on_hover_text(tip).clicked() {
                    if has_app {
                        cap.mode = CaptureMode::Window;
                    } else {
                        open_picker = true;
                    }
                }
                if has_app && ui.small_button("Choose...").on_hover_text("Pick another app").clicked() {
                    open_picker = true;
                }
            });
            if app_on {
                let waiting = status.as_ref().is_some_and(|s| s.app_waiting);
                let (text, col) = if waiting {
                    (format!("Waiting for {}: it's closed, minimized or not responding, so clips show black until it's back.", cap.window_exe), c.warning.0)
                } else {
                    (format!("Only {} is recorded, even when other windows are in front.", cap.window_exe), c.text_dim.0)
                };
                ui.label(RichText::new(text).small().color(col));
                ui.label(dim(shared, "Games in exclusive fullscreen can record black: use borderless or windowed.").small());
            }
        });
        if open_picker {
            self.picker = Some(crate::capture::window::list());
        }
        if cap != shared.settings.capture {
            shared.settings.capture = cap;
            shared.mark_dirty();
        }
    }

    /// "Record one app": the open windows, front first.
    fn picker_dialog(&mut self, ctx: &egui::Context, shared: &mut Shared) {
        let Some(list) = &self.picker else { return };
        let c = shared.theme.colors.clone();
        let mut chosen = None;
        let (mut refresh, mut cancel) = (false, false);
        let resp = egui::Modal::new(egui::Id::unique("app-picker")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.label(RichText::new("Record one app").family(crate::themes::brand_bold_family()).size(shared.theme.fonts.body + 5.0));
            ui.add_space(4.0);
            ui.label(dim(shared, "Only its window is recorded, even when others cover it. Snag finds it again by its program name when it restarts."));
            ui.add_space(10.0);
            egui::ScrollArea::vertical().max_height(340.0).show(ui, |ui| {
                if list.is_empty() {
                    ui.label(dim(shared, "No app windows are open. Start the game, then press Refresh."));
                }
                for w in list {
                    let mut text = egui::text::LayoutJob::default();
                    let font = egui::FontId::proportional(shared.theme.fonts.body);
                    text.append(&w.exe, 0.0, egui::TextFormat::simple(font.clone(), c.text.0));
                    text.append(&format!("   {}", w.title), 0.0, egui::TextFormat::simple(font, c.text_dim.0));
                    let b = egui::Button::new(text).min_size(vec2(ui.available_width(), 36.0)).truncate();
                    if ui.add(b).on_hover_text(&w.title).clicked() {
                        chosen = Some(w.clone());
                    }
                }
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Refresh").clicked() {
                    refresh = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });
        if let Some(w) = chosen {
            let cap = &mut shared.settings.capture;
            cap.mode = CaptureMode::Window;
            cap.window_exe = w.exe;
            cap.window_title = w.title;
            shared.mark_dirty();
            self.picker = None;
        } else if refresh {
            self.picker = Some(crate::capture::window::list());
        } else if cancel || resp.should_close() {
            self.picker = None;
        }
    }

    fn main_column(&mut self, ui: &mut egui::Ui, shared: &mut Shared) -> Option<LiveAction> {
        let status = shared.status.clone().filter(|_| shared.connected);
        let c = shared.theme.colors.clone();
        let theme = shared.theme.clone();
        let recording = status.as_ref().is_some_and(|s| s.recording);

        // Status line: the pill and the buffer switch; "Hide preview" keeps
        // its own room on the right; the dropped count always shows in
        // full; the "what's recorded" text gets the rest, cut with "…".
        ui.horizontal(|ui| {
            match &status {
                Some(s) if s.rec_active => {
                    let what = if s.rec_session_mode { "Recording session" } else { "Recording" };
                    status_pill(ui, &theme, c.error.0, &format!("{what} {}", fmt_duration(s.rec_seconds)), Some(c.error.0));
                }
                Some(s) if s.recording => {
                    status_pill(ui, &theme, c.accent.0, "Replay buffer on", Some(c.accent.0));
                }
                Some(_) => {
                    status_pill(ui, &theme, c.text_dim.0, "Buffer off", None);
                }
                None => {
                    status_pill(ui, &theme, c.text_dim.0, "Not running", None);
                }
            }
            if status.is_some() {
                let tip = if recording {
                    format!("Turn the replay buffer off (it keeps the last {} so you can save it)", fmt_duration(shared.settings.buffer_seconds as f64))
                } else {
                    "Turn the replay buffer on".to_string()
                };
                if switch(ui, &theme, recording, "Replay buffer").on_hover_text(tip).clicked() {
                    shared.outbox.push(UiToCore::ToggleBuffer);
                }
            }
            let info = status.as_ref().filter(|s| s.recording).map(|s| {
                let monitor = match &s.capturing_app {
                    Some(app) => app.clone(),
                    None => s.monitor.split_whitespace().next().unwrap_or("").to_string(),
                };
                (format!("{monitor} · {} · {} fps", s.resolution, s.fps), s.frames_dropped)
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = if shared.settings.ui.live_preview { "Hide preview" } else { "Show preview" };
                if ui.add(egui::Button::new(RichText::new(label).color(c.text_dim.0)).frame(false)).clicked() {
                    shared.settings.ui.live_preview = !shared.settings.ui.live_preview;
                    shared.mark_dirty();
                }
                let Some((text, dropped)) = info else { return };
                // The space between the switch and "Hide preview", left to right.
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    let dropped = (dropped > 0).then(|| RichText::new(format!("{dropped} dropped")).color(c.warning.0));
                    let dropped_w = dropped.as_ref().map_or(0.0, |d| {
                        super::widgets::text_width(ui, d.clone(), egui::TextStyle::Body) + ui.spacing().item_spacing.x
                    });
                    let room = (ui.available_width() - dropped_w).max(0.0);
                    let row_h = ui.available_height();
                    ui.allocate_ui_with_layout(vec2(room, row_h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add(egui::Label::new(dim(shared, text)).truncate());
                    });
                    if let Some(d) = dropped {
                        ui.label(d).on_hover_text("Frames the recorder couldn't produce on time (see core.log)");
                    }
                });
            });
        });
        ui.add_space(12.0);

        // Heights, decided before the preview so it can leave room: the
        // tiles always; the recent clips row only while the preview stays
        // at least MIN_PREVIEW tall (on small windows the preview wins).
        const TILES_H: f32 = 60.0;
        const GAP: f32 = 12.0;
        const MIN_PREVIEW: f32 = 170.0;
        let w = ui.available_width();
        let thumbs = (((w + THUMB_GAP) / 150.0).floor() as usize).clamp(3, MAX_RECENT);
        let thumb_w = (w - THUMB_GAP * (thumbs - 1) as f32) / thumbs as f32;
        let strip_h = STRIP_HEAD_H + thumb_w * 9.0 / 16.0 + THUMB_NAME_H;
        let avail_h = ui.available_height();
        let show_strip = avail_h - TILES_H - GAP - (GAP + 6.0) - strip_h >= MIN_PREVIEW;
        let below = TILES_H + GAP + if show_strip { GAP + 6.0 + strip_h } else { 0.0 };

        // The preview: exactly the recorded picture's shape (no black bars
        // around it that could look like part of the clip).
        let area = vec2(w, (avail_h - below).max(120.0));
        let (pw, ph) = preview_aspect(self.size, status.as_ref().map(|s| s.resolution.as_str()));
        let scale = (area.x / pw).min(area.y / ph);
        let box_size = vec2((pw * scale).round(), (ph * scale).round());
        let (row, _) = ui.allocate_exact_size(vec2(area.x, box_size.y), Sense::hover());
        let rect = Rect::from_center_size(row.center(), box_size);
        let radius = CornerRadius::same((shared.theme.corner_radius * 1.5) as u8);
        ui.painter().rect_filled(rect, radius, c.preview.0);
        if !shared.settings.ui.live_preview {
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::top_down(egui::Align::Center)));
            child.add_space(rect.height() / 2.0 - 30.0);
            child.label(RichText::new("Preview is currently disabled").color(Color32::from_gray(200)));
            child.add_space(6.0);
            let b = egui::Button::new(RichText::new("Enable preview").color(Color32::WHITE)).fill(Color32::from_rgb(0x3a, 0x3c, 0x42));
            if child.add(b).clicked() {
                shared.settings.ui.live_preview = true;
                shared.mark_dirty();
            }
        } else if let (Some(tex), true) = (&self.texture, recording) {
            ui.painter().add(
                egui::epaint::RectShape::filled(rect, radius, Color32::WHITE)
                    .with_texture(tex.id(), Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0))),
            );
            if let Some(app) = status.as_ref().filter(|s| s.app_waiting).and_then(|s| s.capturing_app.clone()) {
                let text = format!("Waiting for {app}...");
                ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(15.0), Color32::from_gray(170));
            }
        } else {
            let text = if !shared.connected {
                "The Snag tray app isn't running"
            } else if recording {
                "Waiting for the first frame..."
            } else {
                "The replay buffer is off"
            };
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(15.0), Color32::from_gray(170));
        }
        ui.add_space(GAP);

        // The three tiles: Save clip (wider), Screenshot, Record.
        let keys = shared.settings.hotkeys.clone();
        let show = |k: &str| crate::hotkeys::display(k);
        let session = status.as_ref().is_some_and(|s| s.rec_session_mode);
        let rec_active = status.as_ref().is_some_and(|s| s.rec_active);
        let red = c.error.0;
        let unit = (w - 2.0 * GAP) / 3.5;
        let mut save_clip = false;
        let mut screenshot = false;
        let mut record = false;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            let held = fmt_duration(shared.settings.buffer_seconds as f64);
            let save = Tile {
                title: "Save clip",
                sub: Some(if recording { format!("Saves the last {held}") } else { "The buffer is off".to_string() }),
                key: Some(show(&keys.clip)),
                glyph: None,
                fill: Some(c.accent.0),
                fg: c.accent_text.0,
                enabled: recording,
            };
            let tip = if recording { format!("Save the last {held} as a clip") } else { "Turn the replay buffer on to save clips".to_string() };
            save_clip = tile(ui, &theme, vec2(unit * 1.5, TILES_H), save).on_hover_text(tip).clicked();

            let shot = Tile { title: "Screenshot", sub: None, key: Some(show(&keys.screenshot)), glyph: None, fill: None, fg: c.text.0, enabled: true };
            screenshot = tile(ui, &theme, vec2(unit, TILES_H), shot).on_hover_text("Save a picture of the screen being recorded").clicked();

            let (title, sub, glyph, fill, fg) = match &status {
                Some(s) if s.rec_active && !session => {
                    ("Stop recording", Some(fmt_duration(s.rec_seconds)), Glyph::Square(Color32::WHITE), Some(red), Color32::WHITE)
                }
                Some(s) if session && s.rec_active => ("Save session", None, Glyph::Dot(red), None, c.text.0),
                _ if session => ("Start session", None, Glyph::Dot(red), None, c.text.0),
                _ => ("Record", None, Glyph::Dot(red), None, c.text.0),
            };
            let key = (!(rec_active && !session)).then(|| show(&keys.record));
            let rec = Tile { title, sub, key, glyph: Some(glyph), fill, fg, enabled: shared.connected };
            let tip = if session {
                "Whole-session mode: Snag records everything; this saves the session so far and keeps recording."
            } else if rec_active {
                "Stop and save the recording"
            } else {
                "Records until you press Stop, then saves the whole recording."
            };
            record = tile(ui, &theme, vec2(unit, TILES_H), rec).on_hover_text(tip).clicked();
        });
        if save_clip {
            shared.outbox.push(UiToCore::SaveClip);
        }
        if screenshot {
            shared.outbox.push(UiToCore::Screenshot);
        }
        if record {
            shared.outbox.push(UiToCore::Record);
        }

        if show_strip {
            ui.add_space(GAP + 6.0);
            self.filmstrip(ui, shared, thumbs, thumb_w)
        } else {
            // Keep the list fresh for when the window grows again.
            self.refresh_recent(ui.ctx(), shared);
            None
        }
    }

    /// Reads the clips folder again next frame (a clip was deleted).
    pub fn rescan(&mut self) {
        self.last_scan = None;
    }

    fn refresh_recent(&mut self, ctx: &egui::Context, shared: &Shared) {
        if self.last_scan.is_none_or(|t| t.elapsed() > Duration::from_secs(4)) {
            self.scan(&shared.settings.clips_dir);
        }
        self.collect_thumbs(ctx);
    }

    /// "Recent clips": the newest `count` clips in a row, `thumb_w` wide;
    /// clicking one opens it in the editor.
    fn filmstrip(&mut self, ui: &mut egui::Ui, shared: &mut Shared, count: usize, thumb_w: f32) -> Option<LiveAction> {
        self.refresh_recent(ui.ctx(), shared);
        let mut action = None;
        let c = shared.theme.colors.clone();
        ui.allocate_ui_with_layout(vec2(ui.available_width(), STRIP_HEAD_H), egui::Layout::left_to_right(egui::Align::Min), |ui| {
            ui.label(RichText::new("Recent clips").family(crate::themes::brand_bold_family()).size(shared.theme.fonts.body + 1.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                if ui.link("See all").on_hover_text("Open the Clips tab").clicked() {
                    action = Some(LiveAction::ShowClips);
                }
            });
        });
        if self.recent.is_empty() {
            ui.label(dim(shared, "Clips you save show up here."));
            return action;
        }
        let thumb = vec2(thumb_w, thumb_w * 9.0 / 16.0);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = THUMB_GAP;
            for r in self.recent.iter().take(count) {
                let (tex, secs) = self.thumbs.get(&r.path).cloned().unwrap_or((None, None));
                let (cell, resp) = ui.allocate_exact_size(vec2(thumb_w, thumb.y + THUMB_NAME_H), Sense::click());
                resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, &r.name));
                let p = ui.painter();
                let rect = Rect::from_min_size(cell.min, thumb);
                p.rect_filled(rect, 8.0, c.preview.0);
                if let Some(t) = &tex {
                    p.add(egui::epaint::RectShape::filled(rect, 8.0, Color32::WHITE).with_texture(t.id(), Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0))));
                }
                if let Some(s) = secs {
                    let badge = p.layout_no_wrap(fmt_duration(s), egui::FontId::proportional(11.0), Color32::WHITE);
                    let b = Rect::from_min_size(rect.max - badge.size() - vec2(12.0, 8.0), badge.size() + vec2(8.0, 2.0));
                    p.rect_filled(b, 4.0, Color32::from_black_alpha(180));
                    p.galley(b.min + vec2(4.0, 1.0), badge, Color32::WHITE);
                }
                if resp.hovered() {
                    p.rect_stroke(rect, 8.0, egui::Stroke::new(2.0, c.accent.0), egui::StrokeKind::Inside);
                }
                let name = super::widgets::one_line(p, r.name.clone(), egui::FontId::proportional(12.0), c.text_dim.0, thumb_w);
                p.galley(pos2(cell.left(), rect.bottom() + 4.0), name, c.text_dim.0);
                if resp.on_hover_text("Open in the editor").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    action = Some(LiveAction::OpenClip(r.path.clone()));
                }
            }
        });
        action
    }

    /// The newest videos in the clips folder (up to MAX_RECENT).
    fn scan(&mut self, dir: &Path) {
        self.last_scan = Some(Instant::now());
        let mut files: Vec<(SystemTime, PathBuf)> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("mp4")))
            .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
            .collect();
        files.sort_by_key(|f| std::cmp::Reverse(f.0));
        self.recent = files
            .into_iter()
            .take(MAX_RECENT)
            .map(|(_, path)| Recent { name: path.file_stem().unwrap_or_default().to_string_lossy().into_owned(), path })
            .collect();
        // Forget thumbnails of clips that dropped off the list.
        let keep: Vec<PathBuf> = self.recent.iter().map(|r| r.path.clone()).collect();
        self.thumbs.retain(|p, _| keep.contains(p));
        for r in &self.recent {
            if !self.thumbs.contains_key(&r.path) {
                self.thumbs.insert(r.path.clone(), (None, None));
                let (tx, _) = self.worker.get_or_insert_with(start_thumb_worker);
                let _ = tx.send(r.path.clone());
            }
        }
    }

    fn collect_thumbs(&mut self, ctx: &egui::Context) {
        let Some((_, rx)) = &self.worker else { return };
        while let Ok(t) = rx.try_recv() {
            let tex = t.rgba.map(|(w, h, px)| {
                let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &px);
                ctx.load_texture(format!("thumb-{}", t.path.display()), img, TextureOptions::LINEAR)
            });
            if let Some(slot) = self.thumbs.get_mut(&t.path) {
                *slot = (tex, t.seconds);
            }
        }
    }
}

/// The preview's shape: the last frame's size, else the resolution the
/// core reports ("1920x1080"), else 16:9 (so the box doesn't change shape
/// when the first frame arrives).
fn preview_aspect(frame: (u32, u32), resolution: Option<&str>) -> (f32, f32) {
    if frame.0 > 0 && frame.1 > 0 {
        return (frame.0 as f32, frame.1 as f32);
    }
    let parsed = resolution.and_then(|r| {
        let (w, h) = r.split_once('x')?;
        let (w, h): (f32, f32) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
        (w > 0.0 && h > 0.0).then_some((w, h))
    });
    parsed.unwrap_or((16.0, 9.0))
}

/// Recent clips: at most this many, and the spacing / text heights of
/// their row.
const MAX_RECENT: usize = 6;
const THUMB_GAP: f32 = 10.0;
const STRIP_HEAD_H: f32 = 28.0;
const THUMB_NAME_H: f32 = 22.0;

enum Glyph {
    Dot(Color32),
    Square(Color32),
}

/// One of the big action tiles under the preview.
struct Tile<'a> {
    title: &'a str,
    /// Second line ("Saves the last 2:00"), before the hotkey.
    sub: Option<String>,
    /// The hotkey, drawn as a key cap.
    key: Option<String>,
    glyph: Option<Glyph>,
    /// Background (else the theme's button color).
    fill: Option<Color32>,
    fg: Color32,
    enabled: bool,
}

/// Draws a tile: title (with an optional dot/square) on top, then the
/// second line and the hotkey cap; text is cut with "…" to fit.
fn tile(ui: &mut egui::Ui, theme: &crate::themes::Theme, size: egui::Vec2, t: Tile) -> egui::Response {
    let c = &theme.colors;
    let (rect, resp) = ui.allocate_exact_size(size, if t.enabled { Sense::click() } else { Sense::hover() });
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, t.enabled, t.title));
    let hovered = t.enabled && resp.hovered();
    let fade = |col: Color32| if t.enabled { col } else { col.gamma_multiply(0.45) };
    let radius = CornerRadius::same((theme.corner_radius * 1.5) as u8);
    let p = ui.painter();
    match t.fill {
        Some(f) => {
            p.rect_filled(rect, radius, fade(if hovered { f.lerp_to_gamma(Color32::WHITE, 0.12) } else { f }));
        }
        None => {
            p.rect_filled(rect, radius, if hovered { c.widget_hover.0 } else { c.widget.0 });
            p.rect_stroke(rect, radius, egui::Stroke::new(1.0, c.border.0), egui::StrokeKind::Inside);
        }
    }
    let pad = 14.0;
    let fg = fade(t.fg);
    let sub_fg = fade(if t.fill.is_some() { t.fg.gamma_multiply(0.8) } else { c.text_dim.0 });
    let two_lines = t.sub.is_some() || t.key.is_some();
    let title_y = if two_lines { rect.center().y - 10.0 } else { rect.center().y };
    let mut x = rect.left() + pad;
    if let Some(g) = &t.glyph {
        let at = pos2(x + 5.0, title_y);
        match g {
            Glyph::Dot(col) => {
                p.circle_filled(at, 5.0, fade(*col));
            }
            Glyph::Square(col) => {
                p.rect_filled(Rect::from_center_size(at, vec2(9.0, 9.0)), 1.5, fade(*col));
            }
        }
        x += 18.0;
    }
    let room = (rect.right() - pad - x).max(10.0);
    let font = egui::FontId::new(theme.fonts.body + 1.0, crate::themes::brand_bold_family());
    let title = super::widgets::one_line(p, t.title.to_string(), font, fg, room);
    p.galley(pos2(x, title_y - title.size().y / 2.0), title, fg);
    if two_lines {
        let y = rect.center().y + 11.0;
        let mut x = rect.left() + pad;
        let right = rect.right() - pad;
        let small = egui::FontId::proportional(theme.fonts.body - 2.0);
        // The key cap keeps its room; the text gets what's left.
        let cap = t.key.map(|k| p.layout_no_wrap(k, egui::FontId::monospace(theme.fonts.body - 3.0), sub_fg));
        let cap_w = cap.as_ref().map_or(0.0, |g| g.size().x + 10.0);
        if let Some(sub) = t.sub {
            let room = right - x - if cap_w > 0.0 { cap_w + 8.0 } else { 0.0 };
            if room > 30.0 {
                let g = super::widgets::one_line(p, sub, small, sub_fg, room);
                let w = g.size().x;
                p.galley(pos2(x, y - g.size().y / 2.0), g, sub_fg);
                x += w + 8.0;
            }
        }
        if let Some(g) = cap {
            if x + cap_w <= right + 0.5 {
                let r = Rect::from_min_size(pos2(x, y - g.size().y / 2.0 - 2.0), vec2(cap_w, g.size().y + 4.0));
                p.rect_stroke(r, 4.0, egui::Stroke::new(1.0, sub_fg.gamma_multiply(0.6)), egui::StrokeKind::Inside);
                p.galley(r.min + vec2(5.0, 2.0), g, sub_fg);
            }
        }
    }
    if t.enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

/// An on/off switch (the replay buffer's).
fn switch(ui: &mut egui::Ui, theme: &crate::themes::Theme, on: bool, label: &str) -> egui::Response {
    let c = &theme.colors;
    let (rect, resp) = ui.allocate_exact_size(vec2(34.0, 18.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::CheckBox, true, on, label));
    let track = if on { c.accent.0 } else { c.widget_active.0 };
    let track = if resp.hovered() { track.lerp_to_gamma(Color32::WHITE, 0.1) } else { track };
    let p = ui.painter();
    p.rect_filled(rect, 9.0, track);
    let knob_x = if on { rect.right() - 9.0 } else { rect.left() + 9.0 };
    p.circle_filled(pos2(knob_x, rect.center().y), 6.5, if on { c.accent_text.0 } else { c.text.0 });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A white card with a bold title (the Daylight look).
fn card(ui: &mut egui::Ui, shared: &mut Shared, title: &str, add: impl FnOnce(&mut egui::Ui, &mut Shared)) {
    let c = shared.theme.colors.clone();
    egui::Frame::new()
        .fill(c.panel.0)
        .stroke(egui::Stroke::new(1.0, c.border.0))
        .corner_radius(CornerRadius::same((shared.theme.corner_radius * 1.5) as u8))
        .inner_margin(egui::Margin::same(18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).family(crate::themes::brand_bold_family()).size(shared.theme.fonts.body + 1.5));
            ui.add_space(10.0);
            add(ui, shared);
        });
}

/// Makes thumbnails (and reads clip lengths) one at a time with ffmpeg,
/// cached as small PNGs in `%LOCALAPPDATA%\Snag\thumbs`.
fn start_thumb_worker() -> (Sender<PathBuf>, Receiver<Thumb>) {
    let (req_tx, req_rx) = channel::<PathBuf>();
    let (res_tx, res_rx) = channel::<Thumb>();
    std::thread::Builder::new()
        .name("thumbnails".into())
        .spawn(move || {
            for path in req_rx {
                let thumb = make_thumb(&path);
                if res_tx.send(thumb).is_err() {
                    break;
                }
            }
        })
        .ok();
    (req_tx, res_rx)
}

fn make_thumb(path: &Path) -> Thumb {
    let seconds = crate::win::hidden_command(crate::paths::ffprobe_exe())
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<f64>().ok());
    let dir = crate::paths::thumbs_dir();
    let _ = std::fs::create_dir_all(&dir);
    let stamp = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = format!("{}-{stamp}.png", path.file_stem().unwrap_or_default().to_string_lossy());
    let png_path = dir.join(name);
    if !png_path.is_file() {
        let at = seconds.map(|s| (s / 3.0).min(2.0)).unwrap_or(0.0);
        let _ = crate::win::hidden_command(crate::paths::ffmpeg_exe())
            .args(["-v", "error", "-nostdin", "-y", "-ss", &format!("{at:.2}"), "-i"])
            .arg(path)
            .args(["-frames:v", "1", "-vf", "scale=320:-2", "-f", "image2", "-c:v", "png"])
            .arg(&png_path)
            .status();
    }
    Thumb { path: path.to_path_buf(), rgba: read_png(&png_path), seconds }
}

pub(super) fn read_png(path: &Path) -> Option<(u32, u32, Vec<u8>)> {
    let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).ok()?));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf[..(w * h * 4) as usize].to_vec(),
        png::ColorType::Rgb => buf[..(w * h * 3) as usize].as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        _ => return None,
    };
    Some((w, h, rgba))
}

#[cfg(test)]
mod tests {
    use super::preview_aspect;

    #[test]
    fn the_preview_takes_the_pictures_shape() {
        assert_eq!(preview_aspect((1280, 1024), Some("1920x1080")), (1280.0, 1024.0));
        assert_eq!(preview_aspect((0, 0), Some("1920x1080")), (1920.0, 1080.0));
        assert_eq!(preview_aspect((0, 0), Some("")), (16.0, 9.0));
        assert_eq!(preview_aspect((0, 0), None), (16.0, 9.0));
    }
}
