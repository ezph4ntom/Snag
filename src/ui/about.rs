//! Settings > About: the version and build, and links (check for updates,
//! release notes, source code, log files).

use eframe::egui::{self, Color32, CornerRadius, RichText, Sense, Stroke, Vec2, pos2, vec2};

use super::Shared;
use crate::update::{CheckStatus, Phase, Updater};

/// The commit this build was made from ("0df40a3") and its date
/// ("17 Nov 2026"); set by build.rs, "unknown" without git.
pub const BUILD_COMMIT: &str = env!("SNAG_BUILD_COMMIT");
pub const BUILD_DATE: &str = env!("SNAG_BUILD_DATE");

#[derive(Default)]
pub struct About {
    icon: Option<egui::TextureHandle>,
}

#[derive(Clone, Copy)]
enum Glyph {
    Refresh,
    External,
    Folder,
}

impl About {
    /// Draws the section.
    pub fn section(&mut self, ui: &mut egui::Ui, shared: &Shared, updater: &mut Updater) {
        let theme = shared.theme.clone();
        let c = theme.colors.clone();
        let icon = self
            .icon
            .get_or_insert_with(|| {
                let px = crate::brand::app_icon_rgba(96, crate::brand::EMBER);
                ui.ctx().load_texture("about-icon", egui::ColorImage::from_rgba_unmultiplied([96, 96], &px), egui::TextureOptions::LINEAR)
            })
            .clone();
        super::section(ui, shared, "About", |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::Image::new((icon.id(), vec2(44.0, 44.0))));
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(
                        RichText::new(format!("Snag {}", updater.version()))
                            .family(crate::themes::brand_bold_family())
                            .size(theme.fonts.body + 1.0),
                    );
                    ui.label(RichText::new("Game clipping for Windows. Built with Rust, egui and ffmpeg.").color(c.text_dim.0));
                    ui.label(RichText::new(format!("Build {BUILD_COMMIT} \u{b7} {BUILD_DATE}")).color(c.text_dim.0).small())
                        .on_hover_text("The commit this copy of Snag was built from");
                });
            });
            ui.add_space(12.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
                let checking = updater.check_status() == CheckStatus::Checking;
                let label = if checking { "Checking" } else { "Check for updates" };
                if link_button(ui, shared, Glyph::Refresh, label, !checking && !updater.busy()).on_hover_text("Ask GitHub for a newer version now").clicked() {
                    updater.check_now();
                }
                if link_button(ui, shared, Glyph::External, "Release notes", true).on_hover_text("What's new in this version (GitHub)").clicked() {
                    crate::win::shell_open(&crate::update::release_notes_url());
                }
                if link_button(ui, shared, Glyph::External, "Source code", true).on_hover_text(crate::update::source_url()).clicked() {
                    crate::win::shell_open(&crate::update::source_url());
                }
                if link_button(ui, shared, Glyph::Folder, "Log files", true).on_hover_text("Attach core.log to bug reports").clicked() {
                    crate::win::reveal_in_explorer(&crate::paths::log_dir());
                }
            });
            status_line(ui, shared, updater);
        });
    }
}

/// What the last "Check for updates" found, and the update itself.
fn status_line(ui: &mut egui::Ui, shared: &Shared, updater: &mut Updater) {
    let c = &shared.theme.colors;
    let text = match (updater.phase(), updater.check_status()) {
        (Phase::Downloading(f), _) => (format!("Downloading the update: {:.0}%", f * 100.0), c.text_dim.0),
        (Phase::Installing, _) => ("Installing the update; Snag restarts by itself.".to_string(), c.text_dim.0),
        (Phase::Failed(e), _) => (format!("The update failed: {e}"), c.error.0),
        (_, CheckStatus::Available(v)) => {
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("Snag {v} is available.")).color(c.success.0));
                let label = if updater.opens_page() { "Open download page" } else { "Update now" };
                let b = egui::Button::new(RichText::new(label).color(c.accent_text.0)).fill(c.accent.0);
                if ui.add(b).clicked() {
                    updater.start();
                }
            });
            return;
        }
        (_, CheckStatus::UpToDate) => ("You're on the latest version.".to_string(), c.success.0),
        (_, CheckStatus::Failed(e)) => (format!("Couldn't check for updates: {e}"), c.error.0),
        _ => return,
    };
    ui.add_space(10.0);
    ui.label(RichText::new(text.0).color(text.1));
}

/// A soft button with a painted icon before its text.
fn link_button(ui: &mut egui::Ui, shared: &Shared, glyph: Glyph, label: &str, enabled: bool) -> egui::Response {
    let c = &shared.theme.colors;
    let font = egui::FontId::proportional(shared.theme.fonts.body - 1.0);
    let fg = if enabled { c.text.0 } else { c.text_dim.0 };
    let galley = ui.painter().layout_no_wrap(label.to_string(), font, fg);
    let size = Vec2::new(galley.size().x + 44.0, 32.0);
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, label));
    let hot = enabled && resp.hovered();
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(8), if hot { c.widget_active.0 } else { c.widget_hover.0 });
    paint_glyph(p, pos2(rect.left() + 18.0, rect.center().y), glyph, fg);
    p.galley(pos2(rect.left() + 32.0, rect.center().y - galley.size().y / 2.0), galley, fg);
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

fn paint_glyph(p: &egui::Painter, c: egui::Pos2, glyph: Glyph, color: Color32) {
    let s = Stroke::new(1.4, color);
    match glyph {
        Glyph::Refresh => {
            // An almost-closed circle with an arrowhead at its end, pointing
            // along the circle.
            let r = 5.5;
            let (from, to) = (-50.0f32, 250.0f32);
            let at = |deg: f32| c + vec2(deg.to_radians().cos(), deg.to_radians().sin()) * r;
            p.add(egui::Shape::line((0..=20).map(|i| at(from + (to - from) * i as f32 / 20.0)).collect(), s));
            let tip = at(to);
            let a = to.to_radians();
            let dir = vec2(-a.sin(), a.cos()); // direction of travel
            let back = |turn: f32| {
                let (sn, cs) = turn.to_radians().sin_cos();
                tip - vec2(dir.x * cs - dir.y * sn, dir.x * sn + dir.y * cs) * 3.6
            };
            p.add(egui::Shape::line(vec![back(40.0), tip, back(-40.0)], s));
        }
        Glyph::External => {
            // A box open at the top right, with an arrow leaving it.
            p.add(egui::Shape::line(vec![c + vec2(-0.5, -5.5), c + vec2(-5.5, -5.5), c + vec2(-5.5, 5.5), c + vec2(5.5, 5.5), c + vec2(5.5, 0.5)], s));
            p.line_segment([c + vec2(-1.0, 1.0), c + vec2(6.0, -6.0)], s);
            p.add(egui::Shape::line(vec![c + vec2(2.0, -6.0), c + vec2(6.0, -6.0), c + vec2(6.0, -2.0)], s));
        }
        Glyph::Folder => {
            p.add(egui::Shape::closed_line(
                vec![c + vec2(-6.5, -4.5), c + vec2(-2.0, -4.5), c + vec2(-0.5, -3.0), c + vec2(6.5, -3.0), c + vec2(6.5, 5.0), c + vec2(-6.5, 5.0)],
                s,
            ));
        }
    }
}
