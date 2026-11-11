//! Appearance tab: pick a theme (applied instantly), plus a live preview.

use eframe::egui::{self, RichText};

use super::{Shared, dim, section};

#[derive(Default)]
pub struct AppearanceTab {
    themes: Option<Vec<crate::themes::ThemeEntry>>,
    demo_slider: f32,
    demo_check: bool,
    demo_text: String,
}

impl AppearanceTab {
    pub fn ui(&mut self, ui: &mut egui::Ui, shared: &mut Shared, theme_error: Option<&str>) {
        let themes = self.themes.get_or_insert_with(crate::themes::list).clone();
        let mut selected = shared.settings.theme.clone();
        let mut reload = false;
        let error_color = shared.theme.colors.error.0;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(760.0);
            section(ui, shared, "Theme", |ui| {
                for t in &themes {
                    ui.radio_value(&mut selected, t.id.clone(), format!("{}   ({}.json)", t.name, t.id));
                }
                if themes.is_empty() {
                    ui.label(format!("No themes found in {}", crate::paths::themes_dir().display()));
                }
                ui.horizontal(|ui| {
                    reload = ui.button("Rescan themes folder").clicked();
                    if ui.button("Open themes folder").clicked() {
                        crate::win::reveal_in_explorer(&crate::paths::themes_dir());
                    }
                });
                if let Some(e) = theme_error {
                    ui.label(RichText::new(format!("This theme has an error, using the built-in theme: {e}")).color(error_color));
                }
                ui.label(dim(
                    shared,
                    "Themes are JSON files (colors, fonts, corner radius, spacing). Edit the active one and it reloads on save. Schema: themes/README.md",
                ));
            });
            section(ui, shared, "Preview", |ui| {
                ui.heading("Heading text");
                ui.label("Body text, and a hyperlink:");
                ui.hyperlink_to("themes/README.md", crate::paths::themes_dir().join("README.md").display().to_string());
                ui.horizontal(|ui| {
                    let _ = ui.button("Button");
                    ui.checkbox(&mut self.demo_check, "Checkbox");
                    ui.add(egui::Slider::new(&mut self.demo_slider, 0.0..=1.0));
                });
                ui.text_edit_singleline(&mut self.demo_text);
                let c = &shared.theme.colors;
                ui.horizontal_wrapped(|ui| {
                    for (name, col) in [
                        ("accent", c.accent.0),
                        ("success", c.success.0),
                        ("warning", c.warning.0),
                        ("error", c.error.0),
                        ("timeline", c.timeline.0),
                        ("selection", c.timeline_selection.0),
                        ("playhead", c.playhead.0),
                    ] {
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
                        ui.painter().rect_filled(rect, 3.0, col);
                        ui.label(RichText::new(name).small());
                    }
                });
            });
        });
        if reload {
            self.themes = None;
        }
        if selected != shared.settings.theme {
            // The App notices the new id next frame and applies it.
            shared.settings.theme = selected;
            shared.mark_dirty();
        }
    }
}
