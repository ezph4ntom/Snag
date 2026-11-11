//! Quality tab: detected hardware, the three auto-tuned presets with their
//! reasoning, manual overrides, and "reset to recommended".

use std::sync::mpsc::{Receiver, channel};

use eframe::egui::{self, RichText};

use super::{Shared, dim, section};
use crate::encode::EncoderKind;
use crate::tune::{self, HardwareInfo, Preset};

#[derive(Default)]
pub struct QualityTab {
    detecting: Option<Receiver<HardwareInfo>>,
}

const HEIGHTS: [u32; 6] = [2160, 1440, 1080, 900, 720, 480];
const FPS: [u32; 7] = [30, 60, 90, 120, 144, 165, 240];

impl QualityTab {
    pub fn ui(&mut self, ui: &mut egui::Ui, shared: &mut Shared) {
        if let Some(rx) = &self.detecting {
            match rx.try_recv() {
                Ok(hw) => {
                    shared.settings.hardware = Some(hw);
                    shared.mark_dirty();
                    self.detecting = None;
                }
                Err(_) => ui.ctx().request_repaint_after(std::time::Duration::from_millis(100)),
            }
        }
        let Some(hw) = shared.settings.hardware.clone() else {
            ui.label("Hardware hasn't been detected yet.");
            self.detect_button(ui);
            return;
        };
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(820.0);
            self.hardware_section(ui, shared, &hw);
            self.preset_section(ui, shared, &hw);
            self.manual_section(ui, shared, &hw);
        });
    }

    fn detect_button(&mut self, ui: &mut egui::Ui) {
        if self.detecting.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Detecting (testing each encoder)...");
            });
        } else if ui.button("Re-detect hardware").clicked() {
            let (tx, rx) = channel();
            std::thread::spawn(move || {
                let _ = tx.send(tune::detect());
            });
            self.detecting = Some(rx);
        }
    }

    fn hardware_section(&mut self, ui: &mut egui::Ui, shared: &Shared, hw: &HardwareInfo) {
        let ok = shared.theme.colors.success.0;
        let off = shared.theme.colors.text_dim.0;
        section(ui, shared, "Detected hardware", |ui| {
            egui::Grid::new("hw").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                ui.label("GPU");
                ui.label(
                    hw.gpus
                        .iter()
                        .map(|g| format!("{} ({} MB)", g.name, g.vram_mb))
                        .collect::<Vec<_>>()
                        .join(", "),
                );
                ui.end_row();
                ui.label("Encoders");
                ui.horizontal_wrapped(|ui| {
                    for k in EncoderKind::ALL {
                        let works = hw.encoders.contains(&k);
                        if works {
                            ui.label(RichText::new(k.label()).color(ok));
                        } else {
                            ui.label(RichText::new(format!("{} (not found)", k.label())).color(off).strikethrough());
                        }
                    }
                });
                ui.end_row();
                ui.label("CPU / RAM");
                ui.label(format!("{} threads, {:.1} GB", hw.cpu_threads, hw.ram_gb));
                ui.end_row();
                ui.label("Monitors");
                ui.label(
                    hw.monitors
                        .iter()
                        .map(|m| format!("{} {}x{} @ {} Hz{}", m.name, m.width, m.height, m.refresh_hz, if m.primary { " (primary)" } else { "" }))
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
                ui.end_row();
            });
            ui.label(dim(shared, format!("{}  ·  detected {}", hw.ffmpeg, hw.detected_at)).small());
            self.detect_button(ui);
        });
    }

    fn preset_section(&mut self, ui: &mut egui::Ui, shared: &mut Shared, hw: &HardwareInfo) {
        let rec = tune::recommend(hw);
        let current = shared.settings.video.preset;
        let accent = shared.theme.colors.accent.0;
        let mut chosen: Option<Preset> = None;
        section(ui, shared, "Preset", |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("Recommended for this PC: {}", rec.preset.label())).strong());
                if ui.button("Reset to recommended").clicked() {
                    chosen = Some(rec.preset);
                }
            });
            for w in &rec.why {
                ui.label(dim(shared, format!("Why: {w}")));
            }
            ui.add_space(6.0);
            ui.columns(3, |cols| {
                for (col, p) in cols.iter_mut().zip(Preset::AUTO) {
                    let plan = tune::plan(hw, p);
                    let (nw, nh, _) = hw.primary_monitor();
                    let h = if plan.output_height == 0 { nh } else { plan.output_height };
                    let w = tune::width_for(nw, nh, h);
                    let selected = current == p;
                    let frame = egui::Frame::new()
                        .stroke(egui::Stroke::new(if selected { 2.0 } else { 1.0 }, if selected { accent } else { shared.theme.colors.border.0 }))
                        .corner_radius(egui::CornerRadius::same(shared.theme.corner_radius as u8))
                        .inner_margin(egui::Margin::same(10));
                    frame.show(col, |ui| {
                        ui.set_width(ui.available_width());
                        let mut title = p.label().to_string();
                        if p == rec.preset {
                            title.push_str("  (recommended)");
                        }
                        if ui.radio(selected, RichText::new(title).strong()).clicked() {
                            chosen = Some(p);
                        }
                        ui.label(format!("{w}x{h} @ {} fps", plan.fps));
                        ui.label(format!("{:.1} Mbps · {}", plan.bitrate_kbps as f64 / 1000.0, plan.encoder.label()));
                    });
                }
            });
            ui.add_space(6.0);
            if current == Preset::Custom {
                ui.label(dim(shared, "Custom: you changed settings by hand (see below)."));
            } else {
                ui.label(RichText::new(format!("Why these {} settings:", current.label())).strong());
                for line in tune::plan(hw, current).why {
                    ui.label(dim(shared, format!("• {line}")));
                }
            }
        });
        if let Some(p) = chosen {
            tune::plan(hw, p).apply_to(&mut shared.settings.video);
            shared.mark_dirty();
        }
    }

    fn manual_section(&mut self, ui: &mut egui::Ui, shared: &mut Shared, hw: &HardwareInfo) {
        let mut v = shared.settings.video.clone();
        let (nw, nh, refresh) = hw.primary_monitor();
        section(ui, shared, "Manual override", |ui| {
            egui::Grid::new("manual").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                ui.label("Encoder");
                egui::ComboBox::from_id_salt("encoder").selected_text(v.encoder.label()).width(260.0).show_ui(ui, |ui| {
                    for k in EncoderKind::ALL {
                        let works = hw.encoders.contains(&k);
                        let label = if works { k.label().to_string() } else { format!("{} (not available)", k.label()) };
                        ui.add_enabled_ui(works, |ui| {
                            ui.selectable_value(&mut v.encoder, k, label);
                        });
                    }
                });
                ui.end_row();

                ui.label("Resolution");
                let label_for = |h: u32| -> String {
                    if h == 0 {
                        format!("Native ({nw}x{nh})")
                    } else {
                        format!("{}x{h}", tune::width_for(nw, nh, h))
                    }
                };
                egui::ComboBox::from_id_salt("res").selected_text(label_for(v.output_height)).width(260.0).show_ui(ui, |ui| {
                    ui.selectable_value(&mut v.output_height, 0, label_for(0));
                    for h in HEIGHTS.into_iter().filter(|&h| h < nh) {
                        ui.selectable_value(&mut v.output_height, h, label_for(h));
                    }
                });
                ui.end_row();

                ui.label("Frame rate");
                egui::ComboBox::from_id_salt("fps").selected_text(format!("{} fps", v.fps)).width(260.0).show_ui(ui, |ui| {
                    for f in FPS.into_iter().filter(|&f| f <= refresh.max(60)) {
                        ui.selectable_value(&mut v.fps, f, format!("{f} fps"));
                    }
                });
                ui.end_row();

                ui.label("Bitrate");
                let mut mbps = v.bitrate_kbps as f32 / 1000.0;
                if ui.add(egui::Slider::new(&mut mbps, 2.0..=80.0).step_by(0.5).suffix(" Mbps")).changed() {
                    v.bitrate_kbps = (mbps * 1000.0) as u32;
                }
                ui.end_row();
            });
            let mb_per_30s = v.bitrate_kbps as f64 * 30.0 / 8.0 / 1000.0;
            ui.label(dim(shared, format!("About {mb_per_30s:.0} MB per 30 s of video.")));
            if !v.encoder.is_hardware() && v.fps > 30 {
                ui.label(RichText::new("x264 above 30 fps can cost a lot of CPU while gaming.").color(shared.theme.colors.warning.0));
            }
            ui.label(dim(shared, "Changing video settings restarts the replay buffer (the current buffer is discarded)."));
        });
        let mut old = shared.settings.video.clone();
        old.preset = v.preset;
        if v != old {
            v.preset = Preset::Custom;
            shared.settings.video = v;
            shared.mark_dirty();
        }
    }
}
