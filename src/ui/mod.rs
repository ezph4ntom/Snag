//! The window: Live (preview), Sound (mixer), Clips (trimmer), Settings,
//! Quality and Appearance tabs, under Snag's own title bar (`chrome.rs`).
//!
//! Runs as its own process (`snag.exe --ui`), started by the core. It
//! reads `CoreStatus` lines on stdin and writes `UiToCore` commands on stdout
//! (see `app::ipc`). Settings are saved straight to settings.json and the
//! core is told to reload them.

mod about;
mod appearance_tab;
mod chrome;
mod dialogs;
mod live_tab;
mod quality_tab;
mod library_tab;
mod search;
mod settings_tab;
mod sound_tab;
mod trimmer_tab;
mod widgets;

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use anyhow::Result;
use eframe::egui::{self, Color32, RichText};

use crate::app::ipc::{CoreStatus, CoreToUi, UiToCore};
use crate::config::Settings;
use crate::themes::Theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Live,
    Sound,
    Clips,
    Settings,
    Quality,
}

impl Tab {
    const ALL: [Tab; 5] = [Tab::Live, Tab::Clips, Tab::Sound, Tab::Quality, Tab::Settings];

    fn parse(s: &str) -> Tab {
        match s {
            "live" => Tab::Live,
            "sound" | "audio" => Tab::Sound,
            "settings" => Tab::Settings,
            "quality" => Tab::Quality,
            "appearance" | "themes" | "about" => Tab::Settings,
            _ => Tab::Clips,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Tab::Live => "Live",
            Tab::Sound => "Sound",
            Tab::Clips => "Clips",
            Tab::Settings => "Settings",
            Tab::Quality => "Quality",
        }
    }

    fn icon(self) -> chrome::Icon {
        match self {
            Tab::Live => chrome::Icon::Live,
            Tab::Sound => chrome::Icon::Sound,
            Tab::Clips => chrome::Icon::Scissors,
            Tab::Settings => chrome::Icon::Gear,
            Tab::Quality => chrome::Icon::Monitor,
        }
    }
}

/// Command-line options of the UI process.
#[derive(Debug, Default)]
pub struct UiArgs {
    pub tab: Option<String>,
    pub clip: Option<PathBuf>,
    pub minimized: bool,
    /// Testing the updater: act as if this were an older version.
    pub pretend_version: Option<String>,
}

impl UiArgs {
    pub fn parse(args: &[String]) -> UiArgs {
        let mut out = UiArgs::default();
        let mut it = args.iter().skip(1);
        while let Some(a) = it.next() {
            match a.as_str() {
                "--tab" => out.tab = it.next().cloned(),
                "--clip" => out.clip = it.next().map(PathBuf::from),
                "--minimized" => out.minimized = true,
                "--pretend-version" => out.pretend_version = it.next().cloned(),
                _ => {}
            }
        }
        out
    }
}

/// Shared state the tabs read and modify.
pub struct Shared {
    pub settings: Settings,
    /// Set when `settings` changed; saved after a short quiet period so a
    /// slider drag writes the file once, not 60 times a second.
    pub dirty_at: Option<Instant>,
    pub theme: Theme,
    pub status: Option<CoreStatus>,
    /// True while the tray (core) process is talking to this window.
    pub connected: bool,
    /// Answer to a hotkey capture request: (target, hotkey or None).
    pub captured_hotkey: Option<(String, Option<String>)>,
    /// Messages for the core, flushed every frame.
    pub outbox: Vec<UiToCore>,
}

impl Shared {
    pub fn mark_dirty(&mut self) {
        self.dirty_at = Some(Instant::now());
    }
}

struct App {
    tab: Tab,
    shared: Shared,
    incoming: Receiver<CoreToUi>,
    connected: bool,
    theme_id: String,
    theme_mtime: Option<std::time::SystemTime>,
    theme_error: Option<String>,
    last_theme_check: Instant,
    trimmer: trimmer_tab::TrimmerTab,
    library: library_tab::LibraryTab,
    /// New versions from GitHub (the "Update to X" button).
    updater: crate::update::Updater,
    /// Settings > About.
    about: about::About,
    /// Closing was asked while uploads run: confirm first.
    confirm_close: bool,
    close_anyway: bool,
    settings_tab: settings_tab::SettingsTab,
    live: live_tab::LiveTab,
    sound: sound_tab::SoundTab,
    quality: quality_tab::QualityTab,
    appearance: appearance_tab::AppearanceTab,
    /// Window to minimize (without focus) once eframe has shown it.
    minimize_pending: Option<isize>,
    /// The header logo, drawn in the theme's text and accent colors.
    logo: Option<(egui::TextureHandle, (Color32, Color32))>,
    /// Whether the core is currently sending preview frames.
    preview_on: bool,
    /// Tab bar being dragged: the edge it would dock to.
    dock_target: Option<crate::config::TabDock>,
    /// Icons-only side bar: open (showing names) while hovered.
    bar_open: bool,
    /// The open side bar's own grip is being dragged.
    bar_grip_drag: bool,
    /// Size of the tab bar last frame (for the snap preview).
    tab_bar_size: egui::Vec2,
    /// The area below the title bar last frame.
    content_rect: egui::Rect,
    /// The latest note from the core and when it appeared (shown briefly
    /// as a toast in the corner).
    toast: Option<(String, Instant)>,
    /// The settings search in the title bar ("" = not searching).
    search: String,
}

pub fn run(args: UiArgs) -> Result<()> {
    crate::logging::init("ui");
    crate::paths::migrate_from_clipapp();
    let (mut settings, _) = Settings::load();
    let tab = Tab::parse(args.tab.as_deref().unwrap_or("live"));
    let open_uploads = args.tab.as_deref() == Some("uploads");
    let pretend_version = args.pretend_version.clone();
    let open_appearance = matches!(args.tab.as_deref(), Some("appearance" | "themes"));
    let open_about = args.tab.as_deref() == Some("about");
    // Themes were renamed (v0.5): a theme that no longer exists becomes the
    // default, Daylight.
    let theme_now = crate::themes::existing_or_default(&settings.theme);
    if theme_now != settings.theme {
        log::info!("theme {:?} doesn't exist any more; using {theme_now}", settings.theme);
        settings.theme = theme_now;
        let _ = settings.save();
    }

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Snag")
        // Snag draws its own title bar (see `chrome.rs`).
        .with_decorations(false)
        .with_inner_size([1120.0, 740.0])
        .with_min_inner_size([820.0, 540.0])
        .with_icon(app_icon());
    if args.minimized {
        // eframe always shows the window after its first frame. Starting it
        // inactive and off-screen makes that show invisible and harmless to
        // a fullscreen game; `minimize_without_focus` then minimizes it.
        viewport = viewport.with_active(false).with_position([-32000.0, -32000.0]);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    let minimized = args.minimized;
    eframe::run_native(
        "Snag",
        options,
        Box::new(move |cc| {
            let hwnd = window_hwnd(cc);
            let ctx = cc.egui_ctx.clone();
            let (theme, theme_error) = crate::themes::load(&settings.theme);
            crate::themes::apply(&ctx, &theme, true);
            // UI size is set with the -/+ buttons in Settings only.
            ctx.options_mut(|o| o.zoom_with_keyboard = false);
            // No built-in fade animations (scrollbars, hover colors): each one
            // repaints the whole window at the monitor's refresh rate (144 Hz
            // here), which made switching to a tall tab cost ~200 ms of CPU.
            ctx.all_styles_mut(|s| s.animation_time = 0.0);
            ctx.set_zoom_factor(settings.ui.scale);
            if let Some(h) = hwnd {
                chrome::install(h, &ctx);
            }

            // stdin reader: status and commands from the core.
            let (tx, incoming) = channel::<CoreToUi>();
            let rctx = ctx.clone();
            std::thread::spawn(move || {
                for line in std::io::stdin().lock().lines().map_while(|l| l.ok()) {
                    if let Ok(msg) = serde_json::from_str::<CoreToUi>(&line) {
                        if tx.send(msg).is_err() {
                            break;
                        }
                        rctx.request_repaint();
                    }
                }
            });

            let mut trimmer = trimmer_tab::TrimmerTab::new(&settings);
            if let Some(clip) = &args.clip {
                trimmer.load(clip.clone());
            }
            let theme_id = settings.theme.clone();
            Ok(Box::new(App {
                tab,
                shared: Shared {
                    settings,
                    dirty_at: None,
                    theme,
                    status: None,
                    connected: false,
                    captured_hotkey: None,
                    outbox: Vec::new(),
                },
                incoming,
                connected: false,
                theme_mtime: crate::themes::mtime(&theme_id),
                theme_id,
                theme_error,
                last_theme_check: Instant::now(),
                trimmer,
                settings_tab: {
                    let mut s = settings_tab::SettingsTab::default();
                    if open_appearance {
                        s.show_category(settings_tab::Category::Appearance);
                    }
                    if open_about {
                        s.show_category(settings_tab::Category::About);
                    }
                    s
                },
                live: live_tab::LiveTab::default(),
                sound: sound_tab::SoundTab::default(),
                quality: quality_tab::QualityTab::default(),
                appearance: appearance_tab::AppearanceTab::default(),
                minimize_pending: if minimized { hwnd } else { None },
                logo: None,
                preview_on: false,
                dock_target: None,
                tab_bar_size: egui::vec2(160.0, 44.0),
                bar_open: false,
                bar_grip_drag: false,
                content_rect: egui::Rect::NOTHING,
                toast: None,
                search: String::new(),
                library: {
                    let mut l = library_tab::LibraryTab::new();
                    if open_uploads {
                        l.show_uploads();
                    }
                    l
                },
                confirm_close: false,
                close_anyway: false,
                updater: crate::update::Updater::new(pretend_version),
                about: about::About::default(),
            }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("window error: {e}"))
}

fn window_hwnd(cc: &eframe::CreationContext<'_>) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match cc.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

/// Minimizes the (off-screen, inactive) window *without* activating it and
/// puts its restore position back on the primary monitor. Win32 directly:
/// `SW_SHOWMINNOACTIVE` adds a taskbar button but leaves focus and z-order
/// alone, so the game in front keeps running undisturbed.
fn minimize_without_focus(hwnd: isize) {
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowPlacement, SW_SHOWMINNOACTIVE, SetWindowPlacement, WINDOWPLACEMENT,
    };
    let hwnd = HWND(hwnd as *mut _);
    let mut wp = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
    // SAFETY: our own top-level window; `wp.length` is set as required.
    unsafe {
        if GetWindowPlacement(hwnd, &mut wp).is_err() {
            return;
        }
        let r = wp.rcNormalPosition;
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        if let Some(m) = crate::capture::primary_monitor() {
            let x = m.x + (m.width as i32 - w).max(0) / 2;
            let y = m.y + (m.height as i32 - h).max(0) / 2;
            wp.rcNormalPosition = RECT { left: x, top: y, right: x + w, bottom: y + h };
        }
        wp.showCmd = SW_SHOWMINNOACTIVE.0 as u32;
        let ok = SetWindowPlacement(hwnd, &wp).is_ok();
        log::info!("window minimized without focus (ok: {ok})");
    }
}

fn app_icon() -> egui::IconData {
    // The Snag app icon (ink rounded square, bitten play, Ember dot).
    const S: u32 = 64;
    egui::IconData { rgba: crate::brand::app_icon_rgba(S, crate::brand::EMBER), width: S, height: S }
}

impl App {
    fn handle_incoming(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.incoming.try_recv() {
            self.connected = true;
            self.shared.connected = true;
            match msg {
                CoreToUi::Status(s) => {
                    let old = self.shared.status.as_ref().map(|o| o.note.clone()).unwrap_or_default();
                    if !s.note.is_empty() && s.note != old {
                        self.toast = Some((s.note.clone(), Instant::now()));
                    }
                    self.shared.status = Some(s);
                }
                CoreToUi::HotkeyCaptured { target, hotkey } => self.shared.captured_hotkey = Some((target, hotkey)),
                CoreToUi::Show { tab, clip, activate } => {
                    self.tab = Tab::parse(&tab);
                    if tab == "uploads" {
                        self.library.show_uploads();
                    }
                    if tab == "appearance" || tab == "themes" {
                        self.settings_tab.show_category(settings_tab::Category::Appearance);
                    }
                    if let Some(c) = clip {
                        self.trimmer.load(c);
                    }
                    if activate {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                }
            }
        }
    }

    fn flush_outbox(&mut self) {
        if self.shared.outbox.is_empty() {
            return;
        }
        let mut out = std::io::stdout().lock();
        for msg in self.shared.outbox.drain(..) {
            // Fails harmlessly when started standalone (no core listening).
            let _ = out.write_all(crate::app::ipc::encode(&msg).as_bytes());
        }
        let _ = out.flush();
    }

    fn save_if_due(&mut self) {
        if let Some(t) = self.shared.dirty_at {
            if t.elapsed() >= Duration::from_millis(400) {
                self.shared.dirty_at = None;
                self.shared.settings.sanitize();
                match self.shared.settings.save() {
                    Ok(()) => self.shared.outbox.push(UiToCore::ReloadSettings),
                    Err(e) => log::error!("saving settings: {e:#}"),
                }
            }
        }
    }

    /// Applies the selected theme when it changes, and re-reads the theme
    /// file when it's edited on disk (checked once a second).
    fn theme_hot_reload(&mut self, ctx: &egui::Context) {
        let wanted = self.shared.settings.theme.clone();
        let changed_selection = wanted != self.theme_id;
        let due = self.last_theme_check.elapsed() >= Duration::from_secs(1);
        if !changed_selection && !due {
            return;
        }
        self.last_theme_check = Instant::now();
        let mtime = crate::themes::mtime(&wanted);
        if changed_selection || mtime != self.theme_mtime {
            let (theme, err) = crate::themes::load(&wanted);
            let fonts_changed = theme.fonts.family != self.shared.theme.fonts.family || changed_selection;
            crate::themes::apply(ctx, &theme, fonts_changed);
            if !changed_selection {
                log::info!("theme {wanted} changed on disk; reloaded");
            }
            self.shared.theme = theme;
            self.theme_error = err;
            self.theme_id = wanted;
            self.theme_mtime = mtime;
        }
    }

    /// The logo texture, redrawn when the theme's colors change: the glyph
    /// takes the text color and the dot follows the accent.
    fn logo(&mut self, ctx: &egui::Context) -> Option<egui::TextureHandle> {
        let c = &self.shared.theme.colors;
        let key = (c.text.0, c.accent.0);
        if self.logo.as_ref().is_none_or(|(_, k)| *k != key) {
            let rgb = |c: Color32| [c.r(), c.g(), c.b()];
            let px = crate::brand::glyph_rgba(64, rgb(c.text.0), rgb(c.accent.0));
            let image = egui::ColorImage::from_rgba_unmultiplied([64, 64], &px);
            let tex = ctx.load_texture("snag-logo", image, egui::TextureOptions::LINEAR);
            self.logo = Some((tex, key));
        }
        self.logo.as_ref().map(|(t, _)| t.clone())
    }

    /// The tab bar on whichever edge it's docked to. Drag its grip (or any
    /// empty part of it) to another edge to move it there.
    fn tab_bar(&mut self, ui: &mut egui::Ui, vertical: bool, inline: bool) {
        let c = self.shared.theme.colors.clone();
        let icons = self.shared.settings.ui.icon_tabs;
        let bar = ui.max_rect();
        // In the title bar, empty space moves the window; elsewhere it drags
        // the tab bar to another edge.
        let (mut dragging, mut released) = (false, false);
        if !inline {
            let bg = ui.interact(bar, egui::Id::unique("tabbar-bg"), egui::Sense::drag());
            dragging = bg.dragged();
            released = bg.drag_stopped();
        }

        let layout = if vertical {
            egui::Layout::top_down(egui::Align::Center)
        } else {
            egui::Layout::left_to_right(egui::Align::Center)
        };
        let mut icon_rects: Vec<(Tab, egui::Rect)> = Vec::new();
        let mut grip_at = egui::Rect::NOTHING;
        ui.with_layout(layout, |ui| {
            let grip_size = if vertical { egui::vec2(28.0, 14.0) } else { egui::vec2(14.0, 28.0) };
            let (grip_rect, grip) = ui.allocate_exact_size(grip_size, egui::Sense::drag());
            grip_at = grip_rect;
            let grip = grip.on_hover_text("Drag to move the tab bar to another edge").on_hover_cursor(egui::CursorIcon::Grab);
            chrome::paint_grip(ui.painter(), grip_rect, vertical, if grip.hovered() { c.text.0 } else { c.text_dim.0 });
            dragging |= grip.dragged();
            released |= grip.drag_stopped();
            for tab in Tab::ALL {
                let selected = self.tab == tab;
                let (fill, fg) = if selected { (c.accent.0, c.accent_text.0) } else { (Color32::TRANSPARENT, c.text.0) };
                let resp = if icons {
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(38.0, 38.0), egui::Sense::click());
                    let bgc = if selected { fill } else if resp.hovered() { c.widget_hover.0 } else { fill };
                    ui.painter().rect_filled(rect, self.shared.theme.corner_radius, bgc);
                    chrome::paint_icon(ui.painter(), rect.shrink(9.0), tab.icon(), fg);
                    icon_rects.push((tab, rect));
                    if !vertical {
                        // Top/bottom bar: the name fades in under the icon.
                        let t = ui.ctx().animate_bool_with_time(egui::Id::unique(("tab-name", tab.label())), resp.hovered(), 0.12);
                        if t > 0.01 {
                            name_popup(ui.ctx(), rect, tab.label(), t, &c);
                        }
                    }
                    resp
                } else if !vertical {
                    // Top/bottom: plain text tabs, the current one underlined.
                    let font = egui::FontId::proportional(self.shared.theme.fonts.body + 0.5);
                    let galley = ui.painter().layout_no_wrap(tab.label().to_string(), font, c.text.0);
                    let h = ui.available_height().clamp(30.0, 52.0);
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(galley.size().x + 22.0, h), egui::Sense::click());
                    let color = if selected || resp.hovered() { c.text.0 } else { c.text_dim.0 };
                    let pos = egui::pos2(rect.center().x - galley.size().x / 2.0, rect.center().y - galley.size().y / 2.0);
                    ui.painter().galley(pos, galley, color);
                    if selected {
                        let y = if self.shared.settings.ui.tab_dock == crate::config::TabDock::Bottom { rect.top() + 1.0 } else { rect.bottom() - 1.0 };
                        let line = egui::Rect::from_center_size(egui::pos2(rect.center().x, y), egui::vec2(rect.width() - 14.0, 2.5));
                        ui.painter().rect_filled(line, 1.0, c.accent.0);
                    }
                    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
                } else {
                    let b = egui::Button::new(RichText::new(tab.label()).color(fg))
                        .fill(fill)
                        .min_size(egui::vec2(ui.available_width(), 34.0));
                    ui.add(b)
                };
                if resp.clicked() {
                    self.tab = tab;
                }
            }
        });

        if icons && vertical {
            let (d, r) = self.slide_out(ui.ctx(), bar.expand(8.0), grip_at, &icon_rects, dragging);
            dragging |= d;
            released |= r;
        }

        if dragging {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            if let Some(p) = ui.ctx().pointer_latest_pos() {
                self.dock_target = Some(chrome::nearest_edge(self.content_rect, p));
            }
        }
        if released {
            if let Some(edge) = self.dock_target.take() {
                if edge != self.shared.settings.ui.tab_dock {
                    self.shared.settings.ui.tab_dock = edge;
                    self.shared.mark_dirty();
                }
            }
        }
    }

    /// Icons-only side bar: while the mouse is over it, a wider copy slides
    /// out over the content (so nothing reflows) with the tab names.
    /// Returns (dragging, released) for its grip: the open bar covers the
    /// real one, so it has its own.
    fn slide_out(&mut self, ctx: &egui::Context, panel: egui::Rect, grip: egui::Rect, icons: &[(Tab, egui::Rect)], dragging: bool) -> (bool, bool) {
        const OPEN_W: f32 = 184.0;
        let left = self.shared.settings.ui.tab_dock == crate::config::TabDock::Left;
        let span = |w: f32| {
            if left {
                egui::Rect::from_min_size(panel.min, egui::vec2(w, panel.height()))
            } else {
                egui::Rect::from_min_max(egui::pos2(panel.max.x - w, panel.min.y), panel.max)
            }
        };
        let hover_w = if self.bar_open { OPEN_W } else { panel.width() };
        self.bar_open = self.bar_grip_drag || (!dragging && ctx.pointer_hover_pos().is_some_and(|p| span(hover_w).contains(p)));
        let t = ctx.animate_bool_with_time(egui::Id::unique("tabbar-open"), self.bar_open, 0.16);
        if t <= 0.01 {
            return (false, false);
        }
        let mut grip_drag = (false, false);
        let ease = 1.0 - (1.0 - t).powi(3); // ease-out
        let rect = span(panel.width() + (OPEN_W - panel.width()) * ease);
        let c = self.shared.theme.colors.clone();
        let radius = self.shared.theme.corner_radius;
        let mut clicked = None;
        egui::Area::new(egui::Id::unique("tabbar-overlay"))
            .order(egui::Order::Foreground)
            .fixed_pos(rect.min)
            .show(ctx, |ui| {
                // Give the area its full size (an empty area is skipped).
                ui.set_min_size(rect.size());
                ui.set_clip_rect(rect.expand(8.0));
                let p = ui.painter().clone();
                p.add(egui::Shape::rect_filled(rect.translate(egui::vec2(if left { 4.0 } else { -4.0 }, 0.0)), 0.0, Color32::from_black_alpha((60.0 * ease) as u8)));
                p.rect_filled(rect, 0.0, c.panel.0);
                let edge_x = if left { rect.right() } else { rect.left() };
                p.line_segment([egui::pos2(edge_x, rect.top()), egui::pos2(edge_x, rect.bottom())], egui::Stroke::new(1.0, c.border.0));
                // The grip row, with its name.
                let grip_row = egui::Rect::from_min_max(egui::pos2(rect.left() + 8.0, grip.top() - 4.0), egui::pos2(rect.right() - 8.0, grip.bottom() + 4.0));
                let g = ui.interact(grip_row, egui::Id::unique("tabbar-overlay-grip"), egui::Sense::drag()).on_hover_cursor(egui::CursorIcon::Grab);
                if g.hovered() || g.dragged() {
                    p.rect_filled(grip_row, radius, c.widget_hover.0);
                }
                let gcol = if g.hovered() || g.dragged() { c.text.0 } else { c.text_dim.0 };
                let grip_here = egui::Rect::from_center_size(egui::pos2(grip.center().x, grip_row.center().y), grip.size());
                chrome::paint_grip(&p, grip_here, true, gcol);
                p.text(
                    egui::pos2(grip_row.left() + 42.0, grip_row.center().y),
                    egui::Align2::LEFT_CENTER,
                    "Drag to move",
                    egui::FontId::proportional(self.shared.theme.fonts.body - 1.5),
                    c.text_dim.0.gamma_multiply(ease),
                );
                grip_drag = (g.dragged(), g.drag_stopped());
                for (tab, icon) in icons {
                    let row = egui::Rect::from_min_max(egui::pos2(rect.left() + 8.0, icon.top()), egui::pos2(rect.right() - 8.0, icon.bottom()));
                    let resp = ui.interact(row, egui::Id::unique(("tab-row", tab.label())), egui::Sense::click());
                    let selected = self.tab == *tab;
                    let (bg, fg) = if selected {
                        (c.accent.0, c.accent_text.0)
                    } else if resp.hovered() {
                        (c.widget_hover.0, c.text.0)
                    } else {
                        (Color32::TRANSPARENT, c.text.0)
                    };
                    p.rect_filled(row, radius, bg);
                    let icon_rect = egui::Rect::from_min_size(egui::pos2(row.left(), row.top()), egui::vec2(38.0, 38.0));
                    chrome::paint_icon(&p, icon_rect.shrink(9.0), tab.icon(), fg);
                    p.text(
                        egui::pos2(icon_rect.right() + 4.0, row.center().y),
                        egui::Align2::LEFT_CENTER,
                        tab.label(),
                        egui::FontId::proportional(self.shared.theme.fonts.body),
                        fg.gamma_multiply(ease),
                    );
                    if resp.clicked() {
                        clicked = Some(*tab);
                    }
                }
            });
        if let Some(tab) = clicked {
            self.tab = tab;
        }
        self.bar_grip_drag = grip_drag.0;
        grip_drag
    }

    /// While the tab bar is dragged: a highlight where it would land.
    fn dock_preview(&self, ctx: &egui::Context) {
        let Some(edge) = self.dock_target else { return };
        let c = &self.shared.theme.colors;
        let size = self.tab_bar_size;
        // A bar moved between horizontal and vertical swaps its thickness.
        let thickness = egui::vec2(size.x.min(size.y).max(56.0).max(if self.shared.settings.ui.icon_tabs { 56.0 } else { 170.0 }), size.y.min(size.x).max(44.0));
        let rect = chrome::dock_rect(self.content_rect, edge, thickness);
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("dock-preview")));
        painter.rect_filled(rect, 6.0, c.accent.0.gamma_multiply(0.25));
        painter.rect_stroke(rect.shrink(1.0), 6.0, egui::Stroke::new(2.0, c.accent.0), egui::StrokeKind::Inside);
    }

    /// While something is typed in the title-bar search: only the matching
    /// settings sections, from every tab, each fully usable.
    fn search_results(&mut self, ui: &mut egui::Ui) {
        let query = self.search.trim().to_string();
        let hits = search::search(&query);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(ui.available_width().min(900.0));
            ui.horizontal(|ui| {
                let n = hits.len();
                let what = if n == 1 { "1 setting".to_string() } else { format!("{n} settings") };
                ui.label(RichText::new(format!("{what} matching \u{201c}{query}\u{201d}")).family(crate::themes::brand_bold_family()).size(self.shared.theme.fonts.body + 2.0));
                if ui.button("Clear").on_hover_text("Esc").clicked() {
                    self.search.clear();
                }
            });
            ui.add_space(10.0);
            if hits.is_empty() {
                ui.label(dim(&self.shared, "Nothing matches. Try words like \u{201c}hotkey\u{201d}, \u{201c}mic\u{201d}, \u{201c}monitor\u{201d}, \u{201c}startup\u{201d} or \u{201c}theme\u{201d}."));
                return;
            }
            if hits.iter().any(|h| matches!(h.hit, search::Hit::Settings(_))) {
                self.settings_tab.poll(ui, &mut self.shared);
            }
            for h in hits {
                ui.label(dim(&self.shared, format!("{}  \u{203a}  {}", h.tab, h.title)).small());
                ui.add_space(2.0);
                match h.hit {
                    search::Hit::Settings(settings_tab::SettingsSection::Uploads) => self.library.settings_section(ui, &mut self.shared),
                    search::Hit::Settings(settings_tab::SettingsSection::About) => self.about.section(ui, &self.shared, &mut self.updater),
                    search::Hit::Settings(s) => self.settings_tab.section_ui(s, ui, &mut self.shared),
                    search::Hit::SoundRecording => self.sound.recording_section(ui, &mut self.shared),
                    search::Hit::SoundSources => self.sound.sources_section(ui, &mut self.shared),
                    search::Hit::QualityHardware => self.quality.section_ui(quality_tab::QualitySection::Hardware, ui, &mut self.shared),
                    search::Hit::QualityPreset => self.quality.section_ui(quality_tab::QualitySection::Preset, ui, &mut self.shared),
                    search::Hit::QualityManual => self.quality.section_ui(quality_tab::QualitySection::Manual, ui, &mut self.shared),
                    search::Hit::Theme => self.appearance.theme_section(ui, &mut self.shared, self.theme_error.as_deref()),
                }
            }
        });
    }

    fn close_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_close {
            return;
        }
        let n = self.library.uploading();
        if n == 0 {
            self.confirm_close = false;
            return;
        }
        let c = self.shared.theme.colors.clone();
        egui::Modal::new(egui::Id::unique("close-uploads")).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.label(RichText::new("Uploads still running").family(crate::themes::brand_bold_family()).size(self.shared.theme.fonts.body + 3.0));
            ui.add_space(6.0);
            let what = if n == 1 { "An upload is".to_string() } else { format!("{n} uploads are") };
            ui.label(format!("{what} still in progress. Closing the window stops it (recording keeps going in the tray)."));
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let keep = egui::Button::new(RichText::new("Keep uploading").color(c.accent_text.0)).fill(c.accent.0);
                if ui.add(keep).clicked() {
                    self.confirm_close = false;
                }
                if ui.button("Stop and close").clicked() {
                    self.library.cancel_uploads();
                    self.close_anyway = true;
                    self.confirm_close = false;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    }

    /// "Update to 0.7.0" left of the search box, while a newer release is
    /// out. Returns true when clicked.
    /// The update button's text, `None` when there's no button.
    fn update_text(phase: &crate::update::Phase, version: &str) -> Option<String> {
        use crate::update::Phase;
        match phase {
            Phase::UpToDate => None,
            Phase::Available => Some(format!("Update to {version}")),
            Phase::Downloading(f) => Some(format!("Downloading {:.0}%", f * 100.0)),
            Phase::Installing => Some("Installing update".to_string()),
            Phase::Failed(_) => Some(format!("Update to {version} failed")),
        }
    }

    /// `compact`: only the icon (a narrow window); the text moves to the
    /// hover tip.
    fn update_button(
        ui: &mut egui::Ui,
        theme: &crate::themes::Theme,
        phase: &crate::update::Phase,
        version: &str,
        this_version: &str,
        opens_page: bool,
        compact: bool,
    ) -> bool {
        use crate::update::Phase;
        let c = &theme.colors;
        let Some(text) = Self::update_text(phase, version) else { return false };
        let (color, tip, fraction) = match phase {
            Phase::UpToDate => return false,
            Phase::Available => (
                c.success.0,
                if opens_page {
                    format!("Snag {version} is out (you have {this_version}). Click to open its download page.")
                } else {
                    format!("Snag {version} is out (you have {this_version}). Click to download and install it; Snag restarts by itself.")
                },
                None,
            ),
            Phase::Downloading(f) => (c.success.0, "Downloading the update from GitHub".to_string(), Some(*f)),
            Phase::Installing => (c.success.0, "Snag closes and starts again when the update is installed".to_string(), None),
            Phase::Failed(e) => (c.error.0, format!("{e}\n\nClick to try again."), None),
        };
        let (shown, tip) = if compact { (String::new(), format!("{text}\n\n{tip}")) } else { (text, tip) };
        let r = widgets::update_pill(ui, theme, &shown, color, fraction).on_hover_text(tip);
        if matches!(phase, Phase::Available | Phase::Failed(_)) {
            r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
        } else {
            false
        }
    }

    /// The status badge in the title bar: recording / buffer state.
    /// Returns true when the replay-buffer badge was clicked (switches
    /// between "120 s" and "2:00").
    fn status_badge(ui: &mut egui::Ui, theme: &crate::themes::Theme, status: Option<&CoreStatus>, minutes: bool) -> bool {
        let c = &theme.colors;
        let Some(s) = status else {
            widgets::status_pill(ui, theme, c.text_dim.0, "Not running", None)
                .on_hover_text("The Snag tray app isn't running, so nothing is recorded. Start snag.exe.");
            return false;
        };
        let mut clicked = false;
        let red = c.error.0;
        if s.rec_active {
            let what = if s.rec_session_mode { "Session" } else { "REC" };
            widgets::status_pill(ui, theme, red, &format!("{what} {}", widgets::fmt_duration(s.rec_seconds)), Some(red))
                .on_hover_text(format!("{} recorded so far", widgets::fmt_bytes(s.rec_bytes)));
        } else if s.recording && s.state == "Recording" {
            let held = if minutes { widgets::fmt_duration(s.buffered_seconds) } else { format!("{:.0} s", s.buffered_seconds) };
            clicked = widgets::status_pill(ui, theme, c.success.0, &format!("Replay buffer · {held}"), None)
                .on_hover_text(if minutes { "Click to show seconds" } else { "Click to show minutes" })
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked();
        } else if s.recording {
            widgets::status_pill(ui, theme, c.warning.0, "Starting...", None);
        } else {
            widgets::status_pill(ui, theme, c.text_dim.0, "Buffer off", None);
        }
        if s.rec_saving {
            ui.spinner().on_hover_text("Saving a recording");
        }
        clicked
    }

    /// The latest note ("Saved Clip_...", errors) for a few seconds, bottom
    /// right.
    fn draw_toast(&mut self, ctx: &egui::Context) {
        let Some((text, since)) = &self.toast else { return };
        let is_error = text.contains("fail") || text.contains("rror") || text.contains("Could not");
        let life = if is_error { 9.0 } else { 4.5 };
        let age = since.elapsed().as_secs_f32();
        if age > life {
            self.toast = None;
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(250));
        let c = &self.shared.theme.colors;
        let screen = ctx.content_rect();
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::unique("toast")));
        let color = if is_error { c.error.0 } else { c.text.0 };
        let galley = painter.layout(text.clone(), egui::FontId::proportional(self.shared.theme.fonts.body), color, 420.0);
        let size = galley.size() + egui::vec2(28.0, 20.0);
        // Top right, just under the title bar (never over the buttons).
        let rect = egui::Rect::from_min_size(egui::pos2(screen.right() - size.x - 18.0, screen.top() + chrome::TITLE_H + 12.0), size);
        painter.rect_filled(rect.translate(egui::vec2(0.0, 2.0)), 10.0, Color32::from_black_alpha(30));
        painter.rect_filled(rect, 10.0, c.panel.0);
        painter.rect_stroke(rect, 10.0, egui::Stroke::new(1.0, if is_error { c.error.0 } else { c.border.0 }), egui::StrokeKind::Inside);
        painter.galley(rect.min + egui::vec2(14.0, 10.0), galley, color);
    }
}

impl eframe::App for App {
    /// Runs even while the window is minimized/hidden (on repaint requests),
    /// so clips sent by the core get loaded in the background.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // eframe shows the window right *after* the first frame is painted;
        // minimizing before that would be undone, so wait until it's visible.
        // (Checked here, not in ui(): eframe stops calling ui() for a window
        // it considers occluded, like our off-screen one.)
        if let Some(hwnd) = self.minimize_pending {
            // SAFETY: querying our own window.
            let shown = unsafe {
                windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(windows::Win32::Foundation::HWND(hwnd as *mut _))
                    .as_bool()
            };
            if shown {
                minimize_without_focus(hwnd);
                self.minimize_pending = None;
            } else {
                ctx.request_repaint_after(Duration::from_millis(16));
            }
        }
        self.handle_incoming(ctx);
        self.save_if_due();
        self.flush_outbox();
        self.theme_hot_reload(ctx);
        self.trimmer.poll_background(&self.shared.settings);
        self.library.poll_background(ctx);
        self.updater.poll(self.shared.settings.ui.check_updates);
        match self.updater.next_check_in() {
            // A check or the download is running: look for its answer (an
            // idle window wouldn't redraw on its own).
            None => ctx.request_repaint_after(Duration::from_millis(200)),
            // Wake an idle window for the next automatic check.
            Some(wait) if self.shared.settings.ui.check_updates => ctx.request_repaint_after(wait + Duration::from_millis(50)),
            Some(_) => {}
        }
        if let Some(t) = self.library.toasts.pop() {
            self.toast = Some((t, Instant::now()));
            self.library.toasts.clear();
        }
        // Closing the window would stop running uploads: ask first.
        if ctx.input(|i| i.viewport().close_requested()) && self.library.uploading() > 0 && !self.close_anyway {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm_close = true;
        }
        // UI size (Settings > Window).
        if (ctx.zoom_factor() - self.shared.settings.ui.scale).abs() > 0.001 {
            ctx.set_zoom_factor(self.shared.settings.ui.scale);
        }
        // Preview frames only while the Live tab is on screen.
        let minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
        let want_preview = self.tab == Tab::Live && self.shared.settings.ui.live_preview && !minimized && self.connected;
        if want_preview != self.preview_on {
            self.preview_on = want_preview;
            self.shared.outbox.push(UiToCore::Preview { on: want_preview });
            self.flush_outbox();
        }
        // Sound levels for the Sound tab and the Live tab's "Sound in clips".
        let searching_sound = !self.search.trim().is_empty();
        self.sound.set_visible((matches!(self.tab, Tab::Sound | Tab::Live) || searching_sound) && !minimized, ctx);
        // Wake up for debounced saves and the theme/clip-list checks.
        ctx.request_repaint_after(Duration::from_millis(if self.shared.dirty_at.is_some() { 100 } else { 1000 }));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        use crate::config::TabDock;
        let ctx = ui.ctx().clone();
        let colors = self.shared.theme.colors.clone();
        let panel = colors.panel.0;
        let margin = egui::Margin::symmetric(8, 6);
        let border = egui::Stroke::new(1.0, colors.border.0);

        let logo = self.logo(&ctx);
        let dock = self.shared.settings.ui.tab_dock;
        let icons = self.shared.settings.ui.icon_tabs;
        let theme = self.shared.theme.clone();
        let status = self.shared.status.clone().filter(|_| self.connected);
        // Ctrl+F jumps into the search; Esc leaves it.
        let focus_search = ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::F));
        if !self.search.is_empty() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.search.clear();
        }
        let mut search = std::mem::take(&mut self.search);
        let badge_minutes = self.shared.settings.ui.badge_minutes;
        let mut toggle_badge = false;
        let update_phase = if self.shared.settings.ui.check_updates || self.updater.busy() {
            self.updater.phase()
        } else {
            crate::update::Phase::UpToDate
        };
        let update_version = self.updater.available().map(|r| r.version.clone()).unwrap_or_default();
        let update_opens_page = self.updater.opens_page();
        let this_version = self.updater.version().to_string();
        let mut start_update = false;
        egui::Panel::top("title")
            .exact_size(chrome::TITLE_H)
            .frame(egui::Frame::new().fill(panel).stroke(egui::Stroke::NONE))
            .show_separator_line(false)
            .show(ui, |ui| {
                let rect = ui.max_rect();
                chrome::title_bar(
                    ui,
                    &colors,
                    logo.as_ref(),
                    |ui| {
                        if dock == TabDock::Top {
                            self.tab_bar(ui, false, true);
                        }
                    },
                    |ui| {
                        if Self::status_badge(ui, &theme, status.as_ref(), badge_minutes) {
                            toggle_badge = true;
                        }
                        ui.add_space(10.0);
                        // In a narrow window, so nothing covers the tabs: the
                        // search box shrinks from 240 to 120 px, then the update
                        // button shows only its icon (text on hover), then the
                        // search box goes down to 72 px.
                        let gaps = 10.0 + 2.0 * ui.spacing().item_spacing.x;
                        let avail = ui.available_width() - gaps;
                        let upd_full = Self::update_text(&update_phase, &update_version).map_or(0.0, |t| widgets::update_pill_width(ui, &theme, &t));
                        let compact = upd_full > 0.0 && avail - upd_full < 120.0;
                        let upd_w = if compact { widgets::update_pill_width(ui, &theme, "") } else { upd_full };
                        let search_w = (avail - upd_w).clamp(72.0, 240.0);
                        widgets::search_box(ui, &theme, &mut search, search_w, focus_search, "Search settings")
                            .on_hover_text("Search all settings (Ctrl+F)");
                        ui.add_space(10.0);
                        if Self::update_button(ui, &theme, &update_phase, &update_version, &this_version, update_opens_page, compact) {
                            start_update = true;
                        }
                    },
                );
                ui.painter().line_segment([rect.left_bottom(), rect.right_bottom()], border);
            });
        self.search = search;
        if start_update {
            self.updater.start();
        }
        if toggle_badge {
            self.shared.settings.ui.badge_minutes = !badge_minutes;
            self.shared.mark_dirty();
        }
        self.content_rect = ui.available_rect_before_wrap();

        let bar_frame = egui::Frame::new().fill(panel).inner_margin(margin).stroke(egui::Stroke::NONE);
        let side_w = if icons { 56.0 } else { 172.0 };
        let r = match dock {
            TabDock::Top => None,
            TabDock::Bottom => Some(egui::Panel::bottom("tabs").frame(bar_frame).exact_size(46.0).show(ui, |ui| self.tab_bar(ui, false, false)).response),
            TabDock::Left => Some(
                egui::Panel::left("tabs-v")
                    .resizable(false)
                    .exact_size(side_w)
                    .frame(bar_frame)
                    .show(ui, |ui| self.tab_bar(ui, true, false))
                    .response,
            ),
            TabDock::Right => Some(
                egui::Panel::right("tabs-v")
                    .resizable(false)
                    .exact_size(side_w)
                    .frame(bar_frame)
                    .show(ui, |ui| self.tab_bar(ui, true, false))
                    .response,
            ),
        };
        self.tab_bar_size = r.map(|r| r.rect.size()).unwrap_or(egui::vec2(600.0, 52.0));
        let bg = colors.background.0;
        let m = self.shared.theme.spacing.window_margin.clamp(0.0, 127.0) as i8 + 6;
        let mut open_clip = None;
        let searching = !self.search.trim().is_empty();
        egui::CentralPanel::default_margins()
            .frame(egui::Frame::new().fill(bg).inner_margin(egui::Margin::symmetric(m + 4, m)))
            .show(ui, |ui| if searching {
                self.search_results(ui);
            } else {
                match self.tab {
                Tab::Live => open_clip = self.live.ui(ui, &mut self.shared, &mut self.sound),
                Tab::Sound => self.sound.ui(ui, &mut self.shared),
                Tab::Clips => {
                    if self.trimmer.is_open() {
                        if self.trimmer.ui(ui, &mut self.shared) {
                            self.trimmer.close();
                        }
                    } else {
                        self.library.trimmer_files = self.trimmer.files_in_use();
                        if let Some(p) = self.library.ui(ui, &mut self.shared) {
                            self.trimmer.load(p);
                        }
                    }
                }
                Tab::Settings => {
                    let library = &mut self.library;
                    let appearance = &mut self.appearance;
                    let about = &mut self.about;
                    let updater = &mut self.updater;
                    let theme_error = self.theme_error.as_deref();
                    self.settings_tab.ui(ui, &mut self.shared, &mut |which, ui, shared| match which {
                        settings_tab::SettingsSection::Uploads => library.settings_section(ui, shared),
                        settings_tab::SettingsSection::Theme => appearance.theme_section(ui, shared, theme_error),
                        settings_tab::SettingsSection::About => about.section(ui, shared, updater),
                        _ => {}
                    });
                }
                Tab::Quality => self.quality.ui(ui, &mut self.shared),
                }
            });
        match open_clip {
            Some(live_tab::LiveAction::OpenClip(path)) => {
                self.trimmer.load(path);
                self.tab = Tab::Clips;
            }
            Some(live_tab::LiveAction::ShowClips) => self.tab = Tab::Clips,
            None => {}
        }
        self.library.modals(&ctx, &self.shared);
        if std::mem::take(&mut self.library.clips_changed) {
            self.live.rescan();
        }
        self.close_dialog(&ctx);
        self.dock_preview(&ctx);
        self.draw_toast(&ctx);
        // A 1 px outline around the whole window (no Windows frame anymore).
        let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        if !maximized {
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("window-outline")));
            painter.rect_stroke(ctx.content_rect(), 0.0, border, egui::StrokeKind::Inside);
        }
        self.flush_outbox();
    }
}

/// A tab's name in a small rounded label under its icon, faded in by `t`.
fn name_popup(ctx: &egui::Context, icon: egui::Rect, name: &str, t: f32, c: &crate::themes::Colors) {
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::unique(("tab-popup", name))));
    let galley = painter.layout_no_wrap(name.to_string(), egui::FontId::proportional(13.0), c.text.0.gamma_multiply(t));
    let size = galley.size() + egui::vec2(14.0, 8.0);
    // Below the icon, or above it when the bar is at the bottom of the window.
    let below = icon.bottom() + 6.0 + size.y < ctx.content_rect().bottom();
    let y = if below { icon.bottom() + 6.0 - 4.0 * (1.0 - t) } else { icon.top() - 6.0 - size.y + 4.0 * (1.0 - t) };
    let rect = egui::Rect::from_min_size(egui::pos2(icon.center().x - size.x / 2.0, y), size);
    painter.rect_filled(rect, 6.0, c.widget.0.gamma_multiply(t));
    painter.rect_stroke(rect, 6.0, egui::Stroke::new(1.0, c.border.0.gamma_multiply(t)), egui::StrokeKind::Inside);
    painter.galley(rect.min + egui::vec2(7.0, 4.0), galley, c.text.0);
}


/// Small helpers shared by the tabs.
pub(crate) fn section(ui: &mut egui::Ui, shared: &Shared, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    let c = &shared.theme.colors;
    egui::Frame::new()
        .fill(c.panel.0)
        .stroke(egui::Stroke::new(1.0, c.border.0))
        .corner_radius(egui::CornerRadius::same((shared.theme.corner_radius * 1.5).clamp(0.0, 255.0) as u8))
        .inner_margin(egui::Margin::same(18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).family(crate::themes::brand_bold_family()).size(shared.theme.fonts.body + 2.0));
            ui.add_space(8.0);
            add(ui);
        });
    ui.add_space(14.0);
}

pub(crate) fn dim(shared: &Shared, text: impl Into<String>) -> RichText {
    RichText::new(text).color(shared.theme.colors.text_dim.0)
}
