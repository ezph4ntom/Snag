//! Window chrome: Snag's own title bar, the tab bar (dockable to any edge),
//! and the tab icons.
//!
//! The window is created without Windows' title bar (`with_decorations
//! (false)`), so we draw one that follows the theme. To keep everything
//! Windows users expect, a small window "subclass" (a hook into the
//! window's message handling) answers Windows' hit-test question "what is
//! under the mouse?":
//! * the outer 6 px are resize borders, so edge/corner resizing is native;
//! * our maximize button reports itself as the maximize button, so Windows
//!   11 shows its Snap Layouts flyout on hover;
//! * dragging the title bar starts Windows' own move loop (Aero Snap to
//!   edges/corners works), and double-clicking it maximizes/restores.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, Sense, Shape, Stroke, Vec2, pos2, vec2};

/// Height of the title bar in points.
pub const TITLE_H: f32 = 36.0;
const BUTTON_W: f32 = 46.0;

// ---- the subclass ------------------------------------------------------------

/// Maximize button in client pixels (left, top, right, bottom); all 0 = none.
static MAX_BTN: [AtomicI32; 4] = [AtomicI32::new(0), AtomicI32::new(0), AtomicI32::new(0), AtomicI32::new(0)];
static MAX_HOVER: AtomicBool = AtomicBool::new(false);
static MAX_PRESSED: AtomicBool = AtomicBool::new(false);
static CTX: OnceLock<egui::Context> = OnceLock::new();

mod hit {
    pub const MAXBUTTON: usize = 9;
    pub const LEFT: isize = 10;
    pub const RIGHT: isize = 11;
    pub const TOP: isize = 12;
    pub const TOPLEFT: isize = 13;
    pub const TOPRIGHT: isize = 14;
    pub const BOTTOM: isize = 15;
    pub const BOTTOMLEFT: isize = 16;
    pub const BOTTOMRIGHT: isize = 17;
}

/// Hooks the window's messages (see the module docs) and asks DWM for
/// Windows 11's rounded corners and the drop shadow.
pub fn install(hwnd: isize, ctx: &egui::Context) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{
        DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmExtendFrameIntoClientArea, DwmSetWindowAttribute,
    };
    use windows::Win32::UI::Controls::MARGINS;
    use windows::Win32::UI::Shell::SetWindowSubclass;
    let _ = CTX.set(ctx.clone());
    let hwnd = HWND(hwnd as *mut _);
    // SAFETY: our own top-level window; the subclass proc is a plain fn.
    unsafe {
        let pref = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const _ as *const _,
            std::mem::size_of_val(&pref) as u32,
        );
        // A 1 px sliver of DWM frame brings back the window shadow.
        let _ = DwmExtendFrameIntoClientArea(hwnd, &MARGINS { cxLeftWidth: 0, cxRightWidth: 0, cyTopHeight: 1, cyBottomHeight: 0 });
        if !SetWindowSubclass(hwnd, Some(subclass_proc), 1, 0).as_bool() {
            log::warn!("title bar: could not hook the window (resizing from the edges won't work)");
        }
    }
}

fn request_repaint() {
    if let Some(c) = CTX.get() {
        c.request_repaint();
    }
}

fn toggle_maximize(hwnd: windows::Win32::Foundation::HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{IsZoomed, SW_MAXIMIZE, SW_RESTORE, ShowWindow};
    // SAFETY: our own window.
    unsafe {
        let _ = ShowWindow(hwnd, if IsZoomed(hwnd).as_bool() { SW_RESTORE } else { SW_MAXIMIZE });
    }
}

unsafe extern "system" fn subclass_proc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wp: windows::Win32::Foundation::WPARAM,
    lp: windows::Win32::Foundation::LPARAM,
    _id: usize,
    _data: usize,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::Foundation::{LRESULT, POINT, RECT};
    use windows::Win32::Graphics::Gdi::ScreenToClient;
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, TrackMouseEvent};
    use windows::Win32::UI::Shell::DefSubclassProc;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, IsZoomed, WM_MOUSEMOVE, WM_NCHITTEST, WM_NCLBUTTONDBLCLK, WM_NCLBUTTONDOWN, WM_NCLBUTTONUP,
        WM_NCMOUSELEAVE, WM_NCMOUSEMOVE,
    };
    // SAFETY (whole body): Win32 queries on our own window, called by
    // Windows on the window's thread.
    unsafe {
        match msg {
            WM_NCHITTEST => {
                let x = (lp.0 & 0xFFFF) as i16 as i32;
                let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
                if !IsZoomed(hwnd).as_bool() {
                    let mut r = RECT::default();
                    let _ = GetWindowRect(hwnd, &mut r);
                    let b = (6 * GetDpiForWindow(hwnd).max(96) / 96) as i32;
                    let (l, rt, t, bt) = (x < r.left + b, x >= r.right - b, y < r.top + b, y >= r.bottom - b);
                    let code = match (l, rt, t, bt) {
                        (true, _, true, _) => Some(hit::TOPLEFT),
                        (_, true, true, _) => Some(hit::TOPRIGHT),
                        (true, _, _, true) => Some(hit::BOTTOMLEFT),
                        (_, true, _, true) => Some(hit::BOTTOMRIGHT),
                        (true, ..) => Some(hit::LEFT),
                        (_, true, ..) => Some(hit::RIGHT),
                        (_, _, true, _) => Some(hit::TOP),
                        (.., true) => Some(hit::BOTTOM),
                        _ => None,
                    };
                    if let Some(c) = code {
                        return LRESULT(c);
                    }
                }
                let mut p = POINT { x, y };
                let _ = ScreenToClient(hwnd, &mut p);
                let [a, b, c, d] = MAX_BTN.each_ref().map(|v| v.load(Ordering::Relaxed));
                if c > a && p.x >= a && p.x < c && p.y >= b && p.y < d {
                    return LRESULT(hit::MAXBUTTON as isize);
                }
            }
            WM_NCMOUSEMOVE => {
                let over = wp.0 == hit::MAXBUTTON;
                if over != MAX_HOVER.swap(over, Ordering::Relaxed) {
                    request_repaint();
                }
                if over {
                    let mut t = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE | TME_NONCLIENT,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    let _ = TrackMouseEvent(&mut t);
                }
            }
            WM_NCMOUSELEAVE | WM_MOUSEMOVE => {
                if MAX_HOVER.swap(false, Ordering::Relaxed) {
                    MAX_PRESSED.store(false, Ordering::Relaxed);
                    request_repaint();
                }
            }
            WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK if wp.0 == hit::MAXBUTTON => {
                MAX_PRESSED.store(true, Ordering::Relaxed);
                request_repaint();
                return LRESULT(0); // no classic button drawing
            }
            WM_NCLBUTTONUP if wp.0 == hit::MAXBUTTON => {
                if MAX_PRESSED.swap(false, Ordering::Relaxed) {
                    toggle_maximize(hwnd);
                }
                return LRESULT(0);
            }
            _ => {}
        }
        DefSubclassProc(hwnd, msg, wp, lp)
    }
}

// ---- title bar ------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Caption {
    Minimize,
    Maximize,
    Close,
}

/// Draws the title bar contents: logo + name on the left (draggable area),
/// minimize/maximize/close on the right.
pub fn title_bar(ui: &mut egui::Ui, colors: &crate::themes::Colors, logo: Option<&egui::TextureHandle>) {
    let ctx = ui.ctx().clone();
    let rect = ui.max_rect();
    let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));

    // The whole bar is a drag handle; widgets added afterwards sit on top.
    let bar = ui.interact(rect, egui::Id::unique("titlebar"), Sense::click_and_drag());
    if bar.double_clicked() {
        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
    } else if bar.drag_started_by(egui::PointerButton::Primary) {
        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }

    // Logo and wordmark.
    let mut x = rect.left() + 12.0;
    if let Some(tex) = logo {
        let r = Rect::from_center_size(pos2(x + 10.0, rect.center().y), vec2(20.0, 20.0));
        ui.painter().image(tex.id(), r, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        x += 28.0;
    }
    let galley = ui.painter().layout_no_wrap(
        "snag".into(),
        egui::FontId::new(19.0, crate::themes::brand_bold_family()),
        colors.text.0,
    );
    ui.painter().galley(pos2(x, rect.center().y - galley.size().y / 2.0), galley, colors.text.0);

    // Caption buttons, right to left.
    let ppp = ctx.pixels_per_point();
    let mut right = rect.right();
    for which in [Caption::Close, Caption::Maximize, Caption::Minimize] {
        let r = Rect::from_min_max(pos2(right - BUTTON_W, rect.top()), pos2(right, rect.bottom()));
        right -= BUTTON_W;
        let resp = ui.interact(r, egui::Id::unique(("caption", which as u8)), Sense::click());
        let native_hover = matches!(which, Caption::Maximize) && MAX_HOVER.load(Ordering::Relaxed);
        let pressed = matches!(which, Caption::Maximize) && MAX_PRESSED.load(Ordering::Relaxed);
        let hovered = resp.hovered() || native_hover;
        let (fill, fg) = match which {
            Caption::Close if hovered => (Color32::from_rgb(0xC4, 0x2B, 0x1C), Color32::WHITE),
            _ if pressed || resp.is_pointer_button_down_on() => (colors.widget_active.0, colors.text.0),
            _ if hovered => (colors.widget_hover.0, colors.text.0),
            _ => (Color32::TRANSPARENT, colors.text.0),
        };
        ui.painter().rect_filled(r, CornerRadius::ZERO, fill);
        paint_caption_glyph(ui.painter(), r.center(), which, maximized, fg);
        if matches!(which, Caption::Maximize) {
            // Tell the subclass where the button is (client pixels).
            for (slot, v) in MAX_BTN.iter().zip([r.left(), r.top(), r.right(), r.bottom()]) {
                slot.store((v * ppp).round() as i32, Ordering::Relaxed);
            }
        }
        let tip = match which {
            Caption::Minimize => "Minimize",
            Caption::Maximize if maximized => "Restore",
            Caption::Maximize => "Maximize",
            Caption::Close => "Close",
        };
        let resp = resp.on_hover_text(tip);
        // Normally the subclass handles maximize clicks natively; this is the
        // fallback if the hook couldn't be installed.
        if resp.clicked() {
            match which {
                Caption::Minimize => ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true)),
                Caption::Maximize => ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized)),
                Caption::Close => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            }
        }
    }
}

fn paint_caption_glyph(p: &egui::Painter, c: Pos2, which: Caption, maximized: bool, color: Color32) {
    let s = Stroke::new(1.0, color);
    match which {
        Caption::Minimize => {
            p.line_segment([c + vec2(-5.0, 0.5), c + vec2(5.0, 0.5)], s);
        }
        Caption::Maximize if maximized => {
            p.rect_stroke(Rect::from_center_size(c + vec2(-1.0, 1.0), vec2(8.0, 8.0)), 1.0, s, egui::StrokeKind::Middle);
            p.line_segment([c + vec2(-3.0, -4.0), c + vec2(4.0, -4.0)], s);
            p.line_segment([c + vec2(4.0, -4.0), c + vec2(4.0, 3.0)], s);
        }
        Caption::Maximize => {
            p.rect_stroke(Rect::from_center_size(c, vec2(10.0, 10.0)), 1.0, s, egui::StrokeKind::Middle);
        }
        Caption::Close => {
            p.line_segment([c + vec2(-5.0, -5.0), c + vec2(5.0, 5.0)], s);
            p.line_segment([c + vec2(-5.0, 5.0), c + vec2(5.0, -5.0)], s);
        }
    }
}

// ---- tab icons -----------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Live,
    Sound,
    Scissors,
    Gear,
    Monitor,
    Palette,
}

fn arc(center: Pos2, r: f32, from: f32, to: f32) -> Vec<Pos2> {
    (0..=16).map(|i| {
        let a = from + (to - from) * i as f32 / 16.0;
        center + vec2(a.cos(), a.sin()) * r
    })
    .collect()
}

/// Draws a line icon in a square `rect`.
pub fn paint_icon(p: &egui::Painter, rect: Rect, icon: Icon, color: Color32) {
    let c = rect.center();
    let u = rect.width() / 20.0; // design grid: 20 x 20
    let s = Stroke::new(1.6 * u.max(0.75), color);
    let at = |x: f32, y: f32| c + vec2(x, y) * u;
    match icon {
        Icon::Live => {
            p.circle_stroke(c, 7.5 * u, s);
            p.circle_filled(c, 3.5 * u, color);
        }
        Icon::Sound => {
            p.add(Shape::convex_polygon(
                vec![at(-7.0, -2.5), at(-3.5, -2.5), at(0.5, -6.5), at(0.5, 6.5), at(-3.5, 2.5), at(-7.0, 2.5)],
                color,
                Stroke::NONE,
            ));
            let q = std::f32::consts::FRAC_PI_4;
            p.add(Shape::line(arc(at(0.5, 0.0), 4.0 * u, -q, q), s));
            p.add(Shape::line(arc(at(0.5, 0.0), 7.5 * u, -q * 1.15, q * 1.15), s));
        }
        Icon::Scissors => {
            p.circle_stroke(at(-4.0, 5.0), 2.6 * u, s);
            p.circle_stroke(at(4.0, 5.0), 2.6 * u, s);
            p.line_segment([at(-2.5, 3.0), at(5.0, -8.0)], s);
            p.line_segment([at(2.5, 3.0), at(-5.0, -8.0)], s);
        }
        Icon::Gear => {
            let mut pts = Vec::new();
            for i in 0..32 {
                let a = i as f32 / 32.0 * std::f32::consts::TAU;
                let tooth = (i / 2) % 2 == 0;
                pts.push(c + vec2(a.cos(), a.sin()) * if tooth { 8.0 * u } else { 6.0 * u });
            }
            p.add(Shape::closed_line(pts, s));
            p.circle_stroke(c, 2.6 * u, s);
        }
        Icon::Monitor => {
            p.rect_stroke(Rect::from_min_max(at(-8.0, -6.5), at(8.0, 4.0)), 1.5 * u, s, egui::StrokeKind::Middle);
            p.line_segment([at(0.0, 4.0), at(0.0, 7.0)], s);
            p.line_segment([at(-4.0, 7.5), at(4.0, 7.5)], s);
        }
        Icon::Palette => {
            p.circle_stroke(c, 7.5 * u, s);
            let half: Vec<Pos2> = arc(c, 7.5 * u, std::f32::consts::FRAC_PI_2, std::f32::consts::FRAC_PI_2 * 3.0);
            p.add(Shape::convex_polygon(half, color, Stroke::NONE));
        }
    }
}

// ---- tab bar docking -------------------------------------------------------------

/// The window edge closest to `p` within `area`.
pub fn nearest_edge(area: Rect, p: Pos2) -> crate::config::TabDock {
    use crate::config::TabDock::*;
    let d = [(p.y - area.top(), Top), (area.bottom() - p.y, Bottom), (p.x - area.left(), Left), (area.right() - p.x, Right)];
    d.into_iter().min_by(|a, b| a.0.total_cmp(&b.0)).map(|(_, e)| e).unwrap_or(Top)
}

/// Where a tab bar docked at `edge` would sit inside `area` (for the snap
/// preview while dragging).
pub fn dock_rect(area: Rect, edge: crate::config::TabDock, thickness: Vec2) -> Rect {
    use crate::config::TabDock::*;
    match edge {
        Top => Rect::from_min_size(area.min, vec2(area.width(), thickness.y)),
        Bottom => Rect::from_min_max(pos2(area.left(), area.bottom() - thickness.y), area.max),
        Left => Rect::from_min_size(area.min, vec2(thickness.x, area.height())),
        Right => Rect::from_min_max(pos2(area.right() - thickness.x, area.top()), area.max),
    }
}

/// A 2x3 dot grip (drag handle).
pub fn paint_grip(p: &egui::Painter, rect: Rect, vertical: bool, color: Color32) {
    let c = rect.center();
    for i in -1..=1 {
        for j in [-1.0f32, 1.0] {
            let off = if vertical { vec2(i as f32 * 4.0, j * 2.0) } else { vec2(j * 2.0, i as f32 * 4.0) };
            p.circle_filled(c + off, 1.3, color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TabDock;

    #[test]
    fn docking_picks_the_closest_edge() {
        let area = Rect::from_min_max(pos2(0.0, 0.0), pos2(1000.0, 600.0));
        assert_eq!(nearest_edge(area, pos2(500.0, 20.0)), TabDock::Top);
        assert_eq!(nearest_edge(area, pos2(500.0, 590.0)), TabDock::Bottom);
        assert_eq!(nearest_edge(area, pos2(10.0, 300.0)), TabDock::Left);
        assert_eq!(nearest_edge(area, pos2(990.0, 300.0)), TabDock::Right);
        let r = dock_rect(area, TabDock::Right, vec2(60.0, 40.0));
        assert_eq!(r, Rect::from_min_max(pos2(940.0, 0.0), pos2(1000.0, 600.0)));
    }
}
