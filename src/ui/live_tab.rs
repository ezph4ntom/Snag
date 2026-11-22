//! Live tab: what's being recorded right now, and the main buttons.
//!
//! Left: status with the replay buffer's on/off switch, the live preview
//! (switchable off: then no preview frames are made at all; resizable from
//! its corner) and three tiles (Save clip / Screenshot / Record), all sized
//! in proportion to the window. Right: "Sources" (what is recorded: a
//! monitor, or only one app) and "Sound in clips" (quick volume sliders).
//!
//! The preview picture comes from the core's recorder (see
//! `capture::preview`), so it shows exactly the monitor being captured and
//! moves with it when the capture follows focus to another monitor.

use eframe::egui::{self, Color32, CornerRadius, Rect, RichText, Sense, TextureOptions, pos2, vec2};

use super::widgets::{fmt_duration, status_pill};
use super::{Shared, dim};
use crate::app::ipc::UiToCore;
use crate::capture::window::AppWindow;
use crate::config::CaptureMode;

/// The smallest the preview can be dragged to, as a share of the largest.
const MIN_PREVIEW_SIZE: f32 = 0.35;

#[derive(Default)]
pub struct LiveTab {
    reader: crate::capture::preview::Reader,
    texture: Option<egui::TextureHandle>,
    size: (u32, u32),
    monitors: Option<Vec<crate::capture::MonitorInfo>>,
    /// The "Record one app" picker is open, with the windows listed when
    /// it opened (only listed then, and on Refresh).
    picker: Option<Vec<AppWindow>>,
}

impl LiveTab {
    pub fn ui(&mut self, ui: &mut egui::Ui, shared: &mut Shared, sound: &mut super::sound_tab::SoundTab) {
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
                self.main_column(ui, shared);
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

    fn main_column(&mut self, ui: &mut egui::Ui, shared: &mut Shared) {
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

        // Proportional layout: the gap and the tiles' height follow the
        // window's height; the preview gets the rest at most, times the
        // size it was dragged to (`ui.live_preview_size`); the tiles are as
        // wide as the preview (but never cramped), centered under it.
        let w = ui.available_width();
        let avail_h = ui.available_height();
        let gap = (avail_h * 0.022).clamp(10.0, 18.0);
        let tiles_h = (avail_h * 0.1).clamp(54.0, 84.0);
        let max_area = vec2(w, (avail_h - tiles_h - gap).max(120.0));
        let (pw, ph) = preview_aspect(self.size, status.as_ref().map(|s| s.resolution.as_str()));
        let fit = (max_area.x / pw).min(max_area.y / ph);
        let max_box = vec2(pw * fit, ph * fit);
        let size = shared.settings.ui.live_preview_size.clamp(MIN_PREVIEW_SIZE, 1.0);
        let box_size = vec2((max_box.x * size).round(), (max_box.y * size).round());

        // The preview and the tiles are one block, centered in the space
        // below the status line (so a smaller preview stays balanced).
        ui.add_space(((avail_h - box_size.y - gap - tiles_h) / 2.0).max(0.0).floor());

        // The preview: exactly the recorded picture's shape (no black bars
        // around it that could look like part of the clip).
        let (row, _) = ui.allocate_exact_size(vec2(w, box_size.y), Sense::hover());
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
        if let Some(new) = resize_grip(ui, rect, max_box, c.accent.0) {
            shared.settings.ui.live_preview_size = new;
            shared.mark_dirty();
        }
        ui.add_space(gap);

        // The three tiles: Save clip (wider), Screenshot, Record.
        let keys = shared.settings.hotkeys.clone();
        let show = |k: &str| crate::hotkeys::display(k);
        let session = status.as_ref().is_some_and(|s| s.rec_session_mode);
        let rec_active = status.as_ref().is_some_and(|s| s.rec_active);
        let red = c.error.0;
        let row_w = box_size.x.max(440.0_f32.min(w));
        let unit = (row_w - 2.0 * gap) / 3.5;
        let mut save_clip = false;
        let mut screenshot = false;
        let mut record = false;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            if w > row_w {
                // Centered under the preview.
                ui.add_space(((w - row_w) / 2.0).floor());
            }
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
            save_clip = tile(ui, &theme, vec2(unit * 1.5, tiles_h), save).on_hover_text(tip).clicked();

            let shot = Tile { title: "Screenshot", sub: None, key: Some(show(&keys.screenshot)), glyph: None, fill: None, fg: c.text.0, enabled: true };
            screenshot = tile(ui, &theme, vec2(unit, tiles_h), shot).on_hover_text("Save a picture of the screen being recorded").clicked();

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
            record = tile(ui, &theme, vec2(unit, tiles_h), rec).on_hover_text(tip).clicked();
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

/// The preview's resize grip, in its bottom-right corner: drag it to make
/// the preview smaller or bigger (the top edge stays put; it stays
/// centered), double-click it for the largest size. Returns the new size
/// (a share of `max_box`, the largest that fits) when it changed.
fn resize_grip(ui: &mut egui::Ui, rect: Rect, max_box: egui::Vec2, accent: Color32) -> Option<f32> {
    let grip = Rect::from_min_size(rect.max - vec2(24.0, 24.0), vec2(24.0, 24.0));
    let resp = ui.interact(grip, egui::Id::unique("live-preview-grip"), Sense::click_and_drag());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, "Resize preview"));
    let active = resp.hovered() || resp.dragged();
    let p = ui.painter();
    // Three short diagonal lines, on a dark backing so they show on any
    // picture.
    p.rect_filled(grip.shrink(3.0), 5.0, Color32::from_black_alpha(if active { 170 } else { 110 }));
    let col = if active { accent } else { Color32::from_white_alpha(200) };
    let corner = grip.max - vec2(7.0, 7.0);
    for d in [5.0, 9.0, 13.0] {
        p.line_segment([corner - vec2(d, 0.0), corner - vec2(0.0, d)], egui::Stroke::new(1.5, col));
    }
    let resp = resp
        .on_hover_cursor(egui::CursorIcon::ResizeNwSe)
        .on_hover_text("Drag to resize the preview. Double-click: largest size.");
    if resp.double_clicked() {
        return Some(1.0);
    }
    if resp.dragged() {
        let at = resp.interact_pointer_pos()?;
        let by_h = (at.y - rect.top()) / max_box.y.max(1.0);
        let by_w = 2.0 * (at.x - rect.center().x) / max_box.x.max(1.0);
        return Some(by_h.max(by_w).clamp(MIN_PREVIEW_SIZE, 1.0));
    }
    None
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
