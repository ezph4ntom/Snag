//! Snag's own small widgets, drawn to match the Daylight design:
//! * [`pill_slider`]: a rounded track, filled in green up to a white
//!   rounded knob (used for volumes and the clip length); [`pill_slider_to`]
//!   goes past 100 % (volume boost);
//! * [`value_box`]: the number next to a slider, with a dimmer unit;
//! * [`status_pill`]: "● Recording" style badges.

use eframe::egui::{self, Color32, CornerRadius, Rect, Response, Sense, Stroke, Vec2, pos2, vec2};

use crate::themes::Theme;

/// A horizontal slider like a modern settings panel: a rounded track with a
/// green fill and a white knob. `value` is 0..=1. Returns the response
/// (`changed()` while dragging or clicking).
pub fn pill_slider(ui: &mut egui::Ui, theme: &Theme, value: &mut f32, width: f32) -> Response {
    pill_slider_to(ui, theme, value, 1.0, width)
}

/// [`pill_slider`] for 0..=`max`. Past 1 (a boost: volumes go to 200 %) the
/// track has a mark at 1, the knob snaps to it when close, the fill turns
/// amber past it, and a double-click puts it back to 1.
pub fn pill_slider_to(ui: &mut egui::Ui, theme: &Theme, value: &mut f32, max: f32, width: f32) -> Response {
    let c = &theme.colors;
    let h = 24.0;
    let knob_w = 20.0;
    let boost = max > 1.0;
    let (rect, mut resp) = ui.allocate_exact_size(vec2(width, h), Sense::click_and_drag());
    let set = |value: &mut f32, v: f32, resp: &mut Response| {
        let v = v.clamp(0.0, max);
        if (v - *value).abs() > f32::EPSILON {
            *value = v;
            resp.mark_changed();
        }
    };
    if let Some(p) = resp.interact_pointer_pos() {
        if resp.dragged() || resp.clicked() || resp.is_pointer_button_down_on() {
            let t = ((p.x - rect.left() - knob_w / 2.0) / (rect.width() - knob_w)).clamp(0.0, 1.0);
            let v = t * max;
            set(value, if boost && (v - 1.0).abs() < 0.03 * max { 1.0 } else { v }, &mut resp);
        }
    }
    if boost && resp.double_clicked() {
        set(value, 1.0, &mut resp);
    }
    // Arrow keys when focused: 5 % steps.
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
            let v = *value + step;
            set(value, v, &mut resp);
        }
    }
    let v = value.clamp(0.0, max) / max;
    let p = ui.painter();
    let radius = CornerRadius::same(8);
    let track = c.control.map(|h| h.0).unwrap_or(c.widget.0);
    p.rect_filled(rect, radius, track);
    let x_at = |t: f32| rect.left() + knob_w / 2.0 + (rect.width() - knob_w) * t;
    // The knob is as tall as the track and drawn last, so the fill ends
    // hidden under it (no color showing around the knob).
    let knob_x = rect.left() + (rect.width() - knob_w) * v;
    let enabled = ui.is_enabled();
    let tone = |col: Color32| if enabled { col } else { col.gamma_multiply(0.4) };
    let fill = Rect::from_min_max(rect.min, pos2(knob_x + knob_w / 2.0, rect.bottom()));
    p.rect_filled(fill, radius, tone(c.success.0));
    if boost {
        let one = x_at(1.0 / max);
        if knob_x + knob_w / 2.0 > one {
            // The boosted part: amber from the 100 % mark to the knob.
            let over = Rect::from_min_max(pos2(one, rect.top()), fill.max);
            p.rect_filled(over, CornerRadius { nw: 0, sw: 0, ne: 8, se: 8 }, tone(c.warning.0));
        }
        let mark = Stroke::new(2.0, Color32::from_white_alpha(if enabled { 110 } else { 50 }));
        p.line_segment([pos2(one, rect.top() + 5.0), pos2(one, rect.bottom() - 5.0)], mark);
    }
    let knob = Rect::from_min_size(pos2(knob_x, rect.top()), vec2(knob_w, h));
    let hovered = resp.hovered() || resp.dragged();
    p.rect_filled(knob.translate(vec2(0.0, 1.0)), radius, Color32::from_black_alpha(45));
    p.rect_filled(knob, radius, if hovered { Color32::from_gray(248) } else { Color32::WHITE });
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
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
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

/// How wide [`update_pill`] is with this text ("" = only the icon).
pub fn update_pill_width(ui: &egui::Ui, theme: &Theme, text: &str) -> f32 {
    if text.is_empty() {
        return 34.0;
    }
    let font = egui::FontId::proportional(theme.fonts.body - 1.0);
    ui.painter().layout_no_wrap(text.to_string(), font, Color32::WHITE).size().x + 40.0
}

/// The title bar's "Update to 0.7.0" button: tinted, outlined, with a
/// download arrow; `fraction` fills it while downloading. An empty `text`
/// makes a round icon-only button (narrow windows).
pub fn update_pill(ui: &mut egui::Ui, theme: &Theme, text: &str, color: Color32, fraction: Option<f32>) -> Response {
    let font = egui::FontId::proportional(theme.fonts.body - 1.0);
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, color);
    let size = Vec2::new(update_pill_width(ui, theme, text), 28.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let hot = resp.hovered() && fraction.is_none();
    let p = ui.painter();
    let radius = CornerRadius::same(14);
    p.rect_filled(rect, radius, color.gamma_multiply(if hot { 0.22 } else { 0.12 }));
    if let Some(f) = fraction {
        let w = (rect.width() * f.clamp(0.0, 1.0)).max(1.0);
        let fill = Rect::from_min_size(rect.min, vec2(w, rect.height()));
        p.with_clip_rect(fill).rect_filled(rect, radius, color.gamma_multiply(0.22));
    }
    p.rect_stroke(rect, radius, Stroke::new(1.0, color.gamma_multiply(if hot { 0.9 } else { 0.6 })), egui::StrokeKind::Inside);
    // A circled arrow pointing down.
    let ic = if text.is_empty() { rect.center() } else { pos2(rect.left() + 17.0, rect.center().y) };
    let s = Stroke::new(1.4, color);
    p.circle_stroke(ic, 6.5, s);
    p.line_segment([ic + vec2(0.0, -3.5), ic + vec2(0.0, 3.0)], s);
    p.line_segment([ic + vec2(-2.8, 0.4), ic + vec2(0.0, 3.2)], s);
    p.line_segment([ic + vec2(2.8, 0.4), ic + vec2(0.0, 3.2)], s);
    p.galley(pos2(rect.left() + 29.0, rect.center().y - galley.size().y / 2.0), galley, color);
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

/// How wide `text` is on one line (no wrapping) in `style`.
pub fn text_width(ui: &egui::Ui, text: impl Into<egui::WidgetText>, style: egui::TextStyle) -> f32 {
    text.into().into_galley(ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, style).size().x
}

/// How wide a plain text button with this label is (padding included, plus
/// a pixel for rounding), to decide a layout before drawing it.
pub fn button_width(ui: &egui::Ui, text: impl Into<egui::WidgetText>) -> f32 {
    text_width(ui, text, egui::TextStyle::Button) + 2.0 * ui.spacing().button_padding.x + 1.0
}

/// A fixed-width, left-aligned label (keeps form rows in columns).
pub fn label_w(ui: &mut egui::Ui, text: &str, width: f32) {
    ui.allocate_ui_with_layout(vec2(width, 28.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_width(width);
        ui.label(text);
    });
}

/// A rounded search field with a magnifier ("Search settings").
pub fn search_box(ui: &mut egui::Ui, theme: &Theme, text: &mut String, width: f32, focus: bool, hint: &str) -> Response {
    let c = &theme.colors;
    // A fixed 30 px tall box, whatever layout it sits in.
    let (rect, _) = ui.allocate_exact_size(vec2(width, 30.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(15), c.widget_hover.0);
    p.rect_stroke(rect, CornerRadius::same(15), Stroke::new(1.0, c.border.0), egui::StrokeKind::Inside);
    let center = pos2(rect.left() + 16.0, rect.center().y - 1.0);
    let s = Stroke::new(1.6, c.text_dim.0);
    p.circle_stroke(center, 5.0, s);
    p.line_segment([center + vec2(3.6, 3.6), center + vec2(7.0, 7.0)], s);
    let edit = egui::TextEdit::singleline(text)
        .hint_text(egui::RichText::new(hint).color(c.text_dim.0))
        .frame(egui::Frame::NONE)
        .align(egui::Align2::LEFT_CENTER)
        .desired_width(width - 42.0);
    let field = Rect::from_min_max(pos2(rect.left() + 30.0, rect.top() + 2.0), pos2(rect.right() - 30.0, rect.bottom() - 2.0));
    // A child ui, so the field doesn't move the cursor back inside the box
    // (in a right-to-left row the next widget would overlap it).
    let resp = ui.new_child(egui::UiBuilder::new().id_salt(hint).max_rect(field).layout(egui::Layout::left_to_right(egui::Align::Center))).add(edit);
    // An "x" that clears the text.
    if !text.is_empty() {
        let xc = pos2(rect.right() - 16.0, rect.center().y);
        let x = ui.interact(Rect::from_center_size(xc, vec2(22.0, 22.0)), resp.id.with("clear"), Sense::click());
        let col = if x.hovered() { c.text.0 } else { c.text_dim.0 };
        if x.hovered() {
            ui.painter().circle_filled(xc, 10.0, c.widget_active.0);
        }
        let s = Stroke::new(1.6, col);
        ui.painter().line_segment([xc + vec2(-4.0, -4.0), xc + vec2(4.0, 4.0)], s);
        ui.painter().line_segment([xc + vec2(4.0, -4.0), xc + vec2(-4.0, 4.0)], s);
        if x.on_hover_text("Clear").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            text.clear();
        }
    }
    if focus {
        resp.request_focus();
    }
    resp
}

/// `text` on one line, cut with "…" to fit `width`.
pub fn one_line(p: &egui::Painter, text: String, font: egui::FontId, color: Color32, width: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text, font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(width);
    p.layout_job(job)
}
