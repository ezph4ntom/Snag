//! Live tab: a preview of what is being recorded right now.
//!
//! The picture comes from the core's recorder (see `capture::preview`), so
//! it shows exactly the monitor being captured and moves with it when the
//! capture follows focus to another monitor.

use std::time::Duration;

use eframe::egui::{self, Color32, Rect, RichText, TextureOptions, pos2, vec2};

use super::{Shared, dim};
use crate::app::ipc::UiToCore;

#[derive(Default)]
pub struct LiveTab {
    reader: crate::capture::preview::Reader,
    texture: Option<egui::TextureHandle>,
    size: (u32, u32),
}

impl LiveTab {
    pub fn ui(&mut self, ui: &mut egui::Ui, shared: &mut Shared) {
        if let Some((w, h, rgba)) = self.reader.next_frame() {
            let image = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
            match &mut self.texture {
                Some(t) => t.set(image, TextureOptions::LINEAR),
                None => self.texture = Some(ui.ctx().load_texture("live-preview", image, TextureOptions::LINEAR)),
            }
            self.size = (w, h);
        }
        ui.ctx().request_repaint_after(Duration::from_millis(66));

        let status = shared.status.clone().filter(|_| shared.connected);
        let recording = status.as_ref().is_some_and(|s| s.recording);
        let c = shared.theme.colors.clone();

        // Header line: what is captured.
        ui.horizontal_wrapped(|ui| {
            match &status {
                Some(s) if s.recording => {
                    ui.label(RichText::new(&s.monitor).strong());
                    ui.label(dim(shared, format!("{} · {} @ {} fps", s.encoder, s.resolution, s.fps)));
                }
                _ => {
                    ui.label(dim(shared, "Not recording"));
                }
            }
        });
        ui.add_space(6.0);

        // The preview, letterboxed into the space left above the footer.
        let footer = 64.0;
        let avail = ui.available_size();
        let area = vec2(avail.x, (avail.y - footer).max(120.0));
        let (rect, _) = ui.allocate_exact_size(area, egui::Sense::hover());
        let (w, h) = if self.size.0 > 0 { (self.size.0 as f32, self.size.1 as f32) } else { (16.0, 9.0) };
        let scale = (area.x / w).min(area.y / h);
        let img = Rect::from_center_size(rect.center(), vec2(w * scale, h * scale));
        let radius = shared.theme.corner_radius.clamp(0.0, 255.0) as u8;
        ui.painter().rect_filled(img, radius, Color32::BLACK);
        match (&self.texture, recording) {
            (Some(tex), true) => {
                ui.painter().image(tex.id(), img, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
            }
            _ => {
                let text = if !shared.connected {
                    "The Snag tray app isn't running"
                } else if recording {
                    "Waiting for the first frame..."
                } else {
                    "The replay buffer is off"
                };
                ui.painter().text(img.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(16.0), c.text_dim.0);
            }
        }
        ui.painter().rect_stroke(img, radius, egui::Stroke::new(1.0, c.border.0), egui::StrokeKind::Outside);

        // Footer: audio sources and the keys.
        ui.add_space(8.0);
        if let Some(s) = &status {
            if s.recording && s.audio {
                let src = if s.audio_sources.is_empty() { "nothing".into() } else { s.audio_sources.join(", ") };
                ui.label(dim(shared, format!("Sound: {src}")));
            } else if s.recording {
                ui.label(dim(shared, "Sound: off"));
            }
        }
        ui.horizontal(|ui| {
            let keys = &shared.settings.hotkeys;
            ui.label(dim(
                shared,
                format!(
                    "{} saves the last {} s  ·  {} takes a screenshot",
                    crate::hotkeys::display(&keys.clip),
                    shared.settings.buffer_seconds,
                    crate::hotkeys::display(&keys.screenshot)
                ),
            ));
            if shared.connected && !recording && ui.button("Start buffer").clicked() {
                shared.outbox.push(UiToCore::ToggleBuffer);
            }
        });
    }
}
