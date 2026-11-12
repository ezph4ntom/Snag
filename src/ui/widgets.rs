//! Snag's own small widgets, drawn to match the Daylight design:
//! * [`pill_slider`]: a rounded track, filled in green up to a white
//!   rounded knob (used for volumes and the clip length);
//! * [`value_box`]: the number next to a slider, with a dimmer unit;
//! * [`status_pill`]: "● Recording" style badges.

use eframe::egui::{self, Color32, CornerRadius, Rect, Response, Sense, Stroke, Vec2, pos2, vec2};

use crate::themes::Theme;

/// A horizontal slider like a modern settings panel: a rounded track with a
/// green fill and a white knob. `value` is 0..=1. Returns the response
/// (`changed()` while dragging or clicking).
pub fn pill_slider(ui: &mut egui::Ui, theme: &Theme, value: &mut f32, width: f32) -> Response {
    let c = &theme.colors;
    let h = 24.0;
    let (rect, mut resp) = ui.allocate_exact_size(vec2(width, h), Sense::click_and_drag());
    if let Some(p) = resp.interact_pointer_pos() {
        if resp.dragged() || resp.clicked() || resp.is_pointer_button_down_on() {
            let knob_w = 16.0;
            let t = ((p.x - rect.left() - knob_w / 2.0) / (rect.width() - knob_w)).clamp(0.0, 1.0);
            if (t - *value).abs() > f32::EPSILON {
                *value = t;
                resp.mark_changed();
            }
        }
    }
    // Arrow keys when focused.
    if resp.has_focus() {
        let step = ui.input(|i| {
            if i.key_pressed(egui::Key::ArrowRight) {
                0.05
            } else if i.key_pressed(egui::Key::ArrowLeft) {
                -0.05
            } else {
                0.0
            }
        });
        if step != 0.0 {
            *value = (*value + step).clamp(0.0, 1.0);
            resp.mark_changed();
        }
    }
    let v = value.clamp(0.0, 1.0);
    let p = ui.painter();
    let radius = CornerRadius::same(8);
    let track = c.control.map(|h| h.0).unwrap_or(c.widget.0);
    p.rect_filled(rect, radius, track);
    let knob_w = 16.0;
    let knob_x = rect.left() + 2.0 + (rect.width() - knob_w - 4.0) * v;
    let fill = Rect::from_min_max(rect.min, pos2(knob_x + knob_w / 2.0 + 2.0, rect.bottom()));
    let green = if ui.is_enabled() { c.success.0 } else { c.success.0.gamma_multiply(0.4) };
    p.rect_filled(fill, radius, green);
    let knob = Rect::from_min_size(pos2(knob_x, rect.top() + 2.0), vec2(knob_w, h - 4.0));
    let hovered = resp.hovered() || resp.dragged();
    p.rect_filled(knob.translate(vec2(0.0, 1.0)), CornerRadius::same(6), Color32::from_black_alpha(40));
    p.rect_filled(knob, CornerRadius::same(6), if hovered { Color32::from_gray(248) } else { Color32::WHITE });
    if resp.has_focus() {
        p.rect_stroke(rect.expand(2.0), CornerRadius::same(10), Stroke::new(1.5, c.accent.0), egui::StrokeKind::Outside);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The value next to a slider: "100" with a dimmer "%" in a rounded box.
pub fn value_box(ui: &mut egui::Ui, theme: &Theme, value: &str, unit: &str, width: f32) {
    let c = &theme.colors;
    let (rect, _) = ui.allocate_exact_size(vec2(width, 28.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(8), c.widget_hover.0);
    let font = egui::FontId::proportional(theme.fonts.body);
    let num = p.layout_no_wrap(value.to_string(), font.clone(), c.text.0);
    let u = p.layout_no_wrap(unit.to_string(), font, c.text_dim.0);
    let total = num.size().x + if unit.is_empty() { 0.0 } else { u.size().x + 3.0 };
    let x = rect.center().x - total / 2.0;
    let y = rect.center().y - num.size().y / 2.0;
    let num_w = num.size().x;
    p.galley(pos2(x, y), num, c.text.0);
    if !unit.is_empty() {
        p.galley(pos2(x + num_w + 3.0, y), u, c.text_dim.0);
    }
}

/// A rounded badge with a colored dot: "● Recording".
pub fn status_pill(ui: &mut egui::Ui, theme: &Theme, dot: Color32, text: &str, tint: Option<Color32>) -> Response {
    let c = &theme.colors;
    let font = egui::FontId::proportional(theme.fonts.body - 1.0);
    let fg = tint.unwrap_or(c.text.0);
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, fg);
    let size = Vec2::new(galley.size().x + 32.0, 28.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let bg = match tint {
        Some(t) => t.gamma_multiply(0.14),
        None => c.widget_hover.0,
    };
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(14), bg);
    p.circle_filled(pos2(rect.left() + 14.0, rect.center().y), 4.0, dot);
    p.galley(pos2(rect.left() + 24.0, rect.center().y - galley.size().y / 2.0), galley, fg);
    resp
}

/// `1:02:03` / `4:05`.
pub fn fmt_duration(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

/// `1.4 GB` / `320 MB`.
pub fn fmt_bytes(bytes: u64) -> String {
    if bytes >= 1 << 30 { format!("{:.1} GB", bytes as f64 / (1u64 << 30) as f64) } else { format!("{:.0} MB", bytes as f64 / (1u64 << 20) as f64) }
}

/// A small shape painted at the start of a button (fonts don't have them).
#[derive(Clone, Copy)]
pub enum Glyph {
    Dot(Color32),
    Square(Color32),
}

/// A button with a painted dot/square before its text ("● Start recording").
pub fn glyph_button(ui: &mut egui::Ui, glyph: Glyph, label: &str, fg: Color32, fill: Option<Color32>, min: Vec2) -> Response {
    let mut b = egui::Button::new(egui::RichText::new(format!("     {label}")).color(fg)).min_size(min);
    if let Some(f) = fill {
        b = b.fill(f);
    }
    let resp = ui.add(b);
    let c = pos2(resp.rect.left() + 20.0, resp.rect.center().y);
    match glyph {
        Glyph::Dot(col) => {
            ui.painter().circle_filled(c, 5.0, col);
        }
        Glyph::Square(col) => {
            ui.painter().rect_filled(Rect::from_center_size(c, vec2(9.0, 9.0)), 2.0, col);
        }
    }
    resp
}

/// A fixed-width, left-aligned label (keeps form rows in columns).
pub fn label_w(ui: &mut egui::Ui, text: &str, width: f32) {
    ui.allocate_ui_with_layout(vec2(width, 28.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_width(width);
        ui.label(text);
    });
}
