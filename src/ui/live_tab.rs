//! Live tab: what's being recorded right now, and the main buttons.
//!
//! Left: status, the live preview (switchable off: then no preview frames
//! are made at all), and Save clip / Screenshot / Record / Stop buffer.
//! Right: "Sound in clips" (quick volume sliders) and the latest clips.
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
}

impl LiveTab {
    pub fn ui(&mut self, ui: &mut egui::Ui, shared: &mut Shared, sound: &mut super::sound_tab::SoundTab) -> Option<LiveAction> {
        let mut action = None;
        let preview_on = shared.settings.ui.live_preview;
        if preview_on {
            if let Some((w, h, rgba)) = self.reader.next_frame() {
                let image = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                match &mut self.texture {
                    Some(t) => t.set(image, TextureOptions::LINEAR),
                    None => self.texture = Some(ui.ctx().load_texture("live-preview", image, TextureOptions::LINEAR)),
                }
                self.size = (w, h);
            }
            ui.ctx().request_repaint_after(Duration::from_millis(33));
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
                    card(ui, shared, "Sound in clips", |ui, shared| sound.compact(ui, shared));
                    ui.add_space(14.0);
                    if let Some(a) = self.recent_clips(ui, shared) {
                        action = Some(a);
                    }
                });
            });
        });
        action
    }

    fn main_column(&mut self, ui: &mut egui::Ui, shared: &mut Shared) {
        let status = shared.status.clone().filter(|_| shared.connected);
        let c = shared.theme.colors.clone();
        let theme = shared.theme.clone();

        // Status line.
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
            if let Some(s) = status.as_ref().filter(|s| s.recording) {
                let monitor = s.monitor.split_whitespace().next().unwrap_or("").to_string();
                ui.label(dim(shared, format!("{monitor} · {} · {} fps · last {} s kept", s.resolution, s.fps, shared.settings.buffer_seconds)));
                if s.frames_dropped > 0 {
                    ui.label(RichText::new(format!("{} dropped", s.frames_dropped)).color(c.warning.0))
                        .on_hover_text("Frames the recorder couldn't produce on time (see core.log)");
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = if shared.settings.ui.live_preview { "Hide preview" } else { "Show preview" };
                if ui.add(egui::Button::new(RichText::new(label).color(c.text_dim.0)).frame(false)).clicked() {
                    shared.settings.ui.live_preview = !shared.settings.ui.live_preview;
                    shared.mark_dirty();
                }
            });
        });
        ui.add_space(12.0);

        // The preview.
        let buttons_h = 44.0;
        let area = vec2(ui.available_width(), (ui.available_height() - buttons_h - 16.0).max(120.0));
        let (rect, _) = ui.allocate_exact_size(area, Sense::hover());
        let radius = CornerRadius::same((shared.theme.corner_radius * 1.5) as u8);
        ui.painter().rect_filled(rect, radius, c.preview.0);
        let recording = status.as_ref().is_some_and(|s| s.recording);
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
            let (w, h) = (self.size.0.max(1) as f32, self.size.1.max(1) as f32);
            let scale = (rect.width() / w).min(rect.height() / h);
            let img = Rect::from_center_size(rect.center(), vec2(w * scale, h * scale));
            ui.painter().image(tex.id(), img, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
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
        ui.add_space(16.0);

        // Buttons.
        let keys = shared.settings.hotkeys.clone();
        let show = |k: &str| crate::hotkeys::display(k);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            let h = egui::vec2(0.0, buttons_h);
            let save = egui::Button::new(RichText::new(format!("Save clip · {}", show(&keys.clip))).color(c.accent_text.0).strong())
                .fill(c.accent.0)
                .min_size(h);
            if ui.add_enabled(recording, save).clicked() {
                shared.outbox.push(UiToCore::SaveClip);
            }
            if ui.add(egui::Button::new(format!("Screenshot · {}", show(&keys.screenshot))).min_size(h)).clicked() {
                shared.outbox.push(UiToCore::Screenshot);
            }
            use super::widgets::{Glyph, glyph_button};
            let red = c.error.0;
            let (label, glyph, fill, fg) = match &status {
                Some(s) if s.rec_active && !s.rec_session_mode => (
                    format!("Stop recording · {}", fmt_duration(s.rec_seconds)),
                    Glyph::Square(Color32::WHITE),
                    Some(red),
                    Color32::WHITE,
                ),
                Some(s) if s.rec_session_mode && s.rec_active => {
                    (format!("Save session · {}", show(&keys.record)), Glyph::Dot(red), None, c.text.0)
                }
                Some(s) if s.rec_session_mode => (format!("Start session recording · {}", show(&keys.record)), Glyph::Dot(red), None, c.text.0),
                _ => (format!("Start recording · {}", show(&keys.record)), Glyph::Dot(red), None, c.text.0),
            };
            let tip = if status.as_ref().is_some_and(|s| s.rec_session_mode) {
                "Whole-session mode: Snag records everything; this saves the session so far and keeps recording."
            } else {
                "Records until you press Stop, then saves the whole recording."
            };
            let resp = ui.add_enabled_ui(shared.connected, |ui| glyph_button(ui, glyph, &label, fg, fill, h)).inner;
            if resp.on_hover_text(tip).clicked() {
                shared.outbox.push(UiToCore::Record);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = if recording { "Stop buffer" } else { "Start buffer" };
                if ui.add_enabled(shared.connected, egui::Button::new(label).min_size(h)).clicked() {
                    shared.outbox.push(UiToCore::ToggleBuffer);
                }
            });
        });
    }

    /// The latest clips with thumbnails; clicking one opens it.
    fn recent_clips(&mut self, ui: &mut egui::Ui, shared: &mut Shared) -> Option<LiveAction> {
        if self.last_scan.is_none_or(|t| t.elapsed() > Duration::from_secs(4)) {
            self.scan(&shared.settings.clips_dir);
        }
        self.collect_thumbs(ui.ctx());
        let mut action = None;
        let c = shared.theme.colors.clone();
        card(ui, shared, "Recent clips", |ui, shared| {
            if self.recent.is_empty() {
                ui.label(dim(shared, "Clips you save show up here."));
                return;
            }
            let col_w = (ui.available_width() - 10.0) / 2.0;
            egui::Grid::new("recent").num_columns(2).spacing([10.0, 10.0]).show(ui, |ui| {
                for (i, r) in self.recent.iter().enumerate() {
                    let (tex, secs) = self.thumbs.get(&r.path).cloned().unwrap_or((None, None));
                    let resp = ui
                        .allocate_ui_with_layout(vec2(col_w, col_w * 9.0 / 16.0 + 26.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                            let (rect, resp) = ui.allocate_exact_size(vec2(col_w, col_w * 9.0 / 16.0), Sense::click());
                            ui.painter().rect_filled(rect, 8.0, c.preview.0);
                            if let Some(t) = &tex {
                                ui.painter().image(t.id(), rect.shrink(0.5), Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                            }
                            if let Some(s) = secs {
                                let badge = ui.painter().layout_no_wrap(fmt_duration(s), egui::FontId::proportional(11.0), Color32::WHITE);
                                let b = Rect::from_min_size(rect.max - badge.size() - vec2(12.0, 8.0), badge.size() + vec2(8.0, 2.0));
                                ui.painter().rect_filled(b, 4.0, Color32::from_black_alpha(180));
                                ui.painter().galley(b.min + vec2(4.0, 1.0), badge, Color32::WHITE);
                            }
                            if resp.hovered() {
                                ui.painter().rect_stroke(rect, 8.0, egui::Stroke::new(2.0, c.accent.0), egui::StrokeKind::Inside);
                            }
                            ui.add(egui::Label::new(RichText::new(&r.name).size(12.5)).truncate());
                            resp
                        })
                        .inner;
                    if resp.on_hover_text("Open in the trimmer").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        action = Some(LiveAction::OpenClip(r.path.clone()));
                    }
                    if i % 2 == 1 {
                        ui.end_row();
                    }
                }
            });
            ui.add_space(4.0);
            if ui.link("See all clips").clicked() {
                action = Some(LiveAction::ShowClips);
            }
        });
        action
    }

    /// The 4 newest videos in the clips folder.
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
            .take(4)
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

fn read_png(path: &Path) -> Option<(u32, u32, Vec<u8>)> {
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
