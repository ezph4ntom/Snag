//! Settings > Appearance: pick a theme (applied instantly).

use eframe::egui::{self, RichText};

use super::{Shared, dim, section};

#[derive(Default)]
pub struct AppearanceTab {
    themes: Option<Vec<crate::themes::ThemeEntry>>,
}

impl AppearanceTab {
    /// The theme picker (also shown by the settings search).
    pub fn theme_section(&mut self, ui: &mut egui::Ui, shared: &mut Shared, theme_error: Option<&str>) {
        let themes = self.themes.get_or_insert_with(crate::themes::list).clone();
        let mut selected = shared.settings.theme.clone();
        let mut reload = false;
        let error_color = shared.theme.colors.error.0;
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
            ui.label(dim(shared, "Themes are JSON files: edit the active one and it reloads on save (see themes/README.md).").small());
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
