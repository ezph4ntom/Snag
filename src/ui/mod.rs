//! The window: Live (preview), Sound (mixer), Clips (trimmer), Settings,
//! Quality and Appearance tabs, under Snag's own title bar (`chrome.rs`).
//!
//! Runs as its own process (`snag.exe --ui`), started by the core. It
//! reads `CoreStatus` lines on stdin and writes `UiToCore` commands on stdout
//! (see `app::ipc`). Settings are saved straight to settings.json and the
//! core is told to reload them.

mod appearance_tab;
mod chrome;
mod dialogs;
mod live_tab;
mod quality_tab;
mod settings_tab;
mod sound_tab;
mod trimmer_tab;

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
    Appearance,
}

impl Tab {
    const ALL: [Tab; 6] = [Tab::Live, Tab::Sound, Tab::Clips, Tab::Settings, Tab::Quality, Tab::Appearance];

    fn parse(s: &str) -> Tab {
        match s {
            "live" => Tab::Live,
            "sound" | "audio" => Tab::Sound,
            "settings" => Tab::Settings,
            "quality" => Tab::Quality,
            "appearance" | "themes" => Tab::Appearance,
            _ => Tab::Clips,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Tab::Live => "Live",
            Tab::Sound => "Sound",
            Tab::Clips => "Clips & trimmer",
            Tab::Settings => "Settings",
            Tab::Quality => "Quality",
            Tab::Appearance => "Appearance",
        }
    }

    fn icon(self) -> chrome::Icon {
        match self {
            Tab::Live => chrome::Icon::Live,
            Tab::Sound => chrome::Icon::Sound,
            Tab::Clips => chrome::Icon::Scissors,
            Tab::Settings => chrome::Icon::Gear,
            Tab::Quality => chrome::Icon::Monitor,
            Tab::Appearance => chrome::Icon::Palette,
        }
    }
}

/// Command-line options of the UI process.
#[derive(Debug, Default)]
pub struct UiArgs {
    pub tab: Option<String>,
    pub clip: Option<PathBuf>,
    pub minimized: bool,
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
    /// Size of the tab bar last frame (for the snap preview).
    tab_bar_size: egui::Vec2,
    /// The area below the title bar last frame.
    content_rect: egui::Rect,
}

pub fn run(args: UiArgs) -> Result<()> {
    crate::logging::init("ui");
    crate::paths::migrate_from_clipapp();
    let (settings, _) = Settings::load();
    let tab = Tab::parse(args.tab.as_deref().unwrap_or("clips"));

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
                settings_tab: settings_tab::SettingsTab::default(),
                live: live_tab::LiveTab::default(),
                sound: sound_tab::SoundTab::default(),
                quality: quality_tab::QualityTab::default(),
                appearance: appearance_tab::AppearanceTab::default(),
                minimize_pending: if minimized { hwnd } else { None },
                logo: None,
                preview_on: false,
                dock_target: None,
                tab_bar_size: egui::vec2(160.0, 44.0),
                content_rect: egui::Rect::NOTHING,
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
                CoreToUi::Status(s) => self.shared.status = Some(s),
                CoreToUi::HotkeyCaptured { target, hotkey } => self.shared.captured_hotkey = Some((target, hotkey)),
                CoreToUi::Show { tab, clip, activate } => {
                    self.tab = Tab::parse(&tab);
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
    fn tab_bar(&mut self, ui: &mut egui::Ui, vertical: bool) {
        let c = self.shared.theme.colors.clone();
        let icons = self.shared.settings.ui.icon_tabs;
        let bar = ui.max_rect();
        let bg = ui.interact(bar, egui::Id::unique("tabbar-bg"), egui::Sense::drag());
        let mut dragging = bg.dragged();
        let mut released = bg.drag_stopped();

        let layout = if vertical {
            egui::Layout::top_down(egui::Align::Center)
        } else {
            egui::Layout::left_to_right(egui::Align::Center)
        };
        ui.with_layout(layout, |ui| {
            let grip_size = if vertical { egui::vec2(28.0, 14.0) } else { egui::vec2(14.0, 28.0) };
            let (grip_rect, grip) = ui.allocate_exact_size(grip_size, egui::Sense::drag());
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
                    resp.on_hover_text(tab.label())
                } else {
                    let mut b = egui::Button::new(RichText::new(tab.label()).color(fg)).fill(fill);
                    if vertical {
                        b = b.min_size(egui::vec2(ui.available_width(), 30.0));
                    }
                    ui.add(b)
                };
                if resp.clicked() {
                    self.tab = tab;
                }
            }
        });

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

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let c = self.shared.theme.colors.clone();
        let Some(s) = self.shared.status.clone().filter(|_| self.connected) else {
            ui.horizontal(|ui| {
                status_dot(ui, c.text_dim.0);
                ui.label(RichText::new("Not connected to the Snag tray process (start snag.exe to record)").color(c.text_dim.0));
            });
            return;
        };
        // Buttons first (right-aligned), so the text gets the remaining width
        // and is shortened with "..." instead of running under the buttons.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let label = if s.recording { "Stop buffer" } else { "Start buffer" };
            if ui.button(label).clicked() {
                self.shared.outbox.push(UiToCore::ToggleBuffer);
            }
            if ui.add_enabled(s.recording, egui::Button::new("Save clip now")).clicked() {
                self.shared.outbox.push(UiToCore::SaveClip);
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                // The recording dot is the logo's dot: the theme accent.
                let color = if s.recording && s.state == "Recording" {
                    c.accent.0
                } else if s.recording {
                    c.warning.0
                } else {
                    c.text_dim.0
                };
                status_dot(ui, color);
                ui.label(&s.state);
                if s.recording && s.frames_dropped > 0 {
                    ui.label(RichText::new(format!("{} dropped", s.frames_dropped)).color(c.warning.0))
                        .on_hover_text("Frames the recorder couldn't produce on time (filled with repeats; see core.log)");
                }
                let mut text = String::new();
                if s.recording {
                    text = format!(
                        "{} · {} @ {} fps{} · {} · {:.0} s buffered",
                        s.encoder,
                        s.resolution,
                        s.fps,
                        if s.audio { " + audio" } else { "" },
                        s.monitor,
                        s.buffered_seconds
                    );
                }
                if !s.note.is_empty() {
                    if !text.is_empty() {
                        text.push_str("   |   ");
                    }
                    text.push_str(&s.note);
                }
                let col = if s.note.contains("fail") || s.note.contains("rror") { c.error.0 } else { c.text_dim.0 };
                ui.add(egui::Label::new(RichText::new(text).color(col)).truncate());
            });
        });
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
        // UI size (Settings > Window).
        if (ctx.zoom_factor() - self.shared.settings.ui.scale).abs() > 0.001 {
            ctx.set_zoom_factor(self.shared.settings.ui.scale);
        }
        // Preview frames only while the Live tab is on screen.
        let minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
        let want_preview = self.tab == Tab::Live && !minimized && self.connected;
        if want_preview != self.preview_on {
            self.preview_on = want_preview;
            self.shared.outbox.push(UiToCore::Preview { on: want_preview });
            self.flush_outbox();
        }
        self.sound.set_visible(self.tab == Tab::Sound && !minimized, ctx);
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
        egui::Panel::top("title")
            .exact_size(chrome::TITLE_H)
            .frame(egui::Frame::new().fill(panel))
            .show_separator_line(false)
            .show(ui, |ui| chrome::title_bar(ui, &colors, logo.as_ref()));
        self.content_rect = ui.available_rect_before_wrap();

        let dock = self.shared.settings.ui.tab_dock;
        let icons = self.shared.settings.ui.icon_tabs;
        let bar_frame = egui::Frame::new().fill(panel).inner_margin(margin).stroke(egui::Stroke::NONE);
        let side_w = if icons { 56.0 } else { 172.0 };
        let r = match dock {
            TabDock::Top => egui::Panel::top("tabs").frame(bar_frame).show(ui, |ui| self.tab_bar(ui, false)).response,
            TabDock::Bottom => egui::Panel::bottom("tabs").frame(bar_frame).show(ui, |ui| self.tab_bar(ui, false)).response,
            TabDock::Left => egui::Panel::left("tabs-v")
                .resizable(false)
                .exact_size(side_w)
                .frame(bar_frame)
                .show(ui, |ui| self.tab_bar(ui, true))
                .response,
            TabDock::Right => egui::Panel::right("tabs-v")
                .resizable(false)
                .exact_size(side_w)
                .frame(bar_frame)
                .show(ui, |ui| self.tab_bar(ui, true))
                .response,
        };
        self.tab_bar_size = r.rect.size();
        egui::Panel::bottom("status")
            .frame(egui::Frame::new().fill(panel).inner_margin(margin))
            .show(ui, |ui| self.status_bar(ui));
        let bg = colors.background.0;
        let m = self.shared.theme.spacing.window_margin.clamp(0.0, 127.0) as i8;
        egui::CentralPanel::default_margins()
            .frame(egui::Frame::new().fill(bg).inner_margin(egui::Margin::same(m)))
            .show(ui, |ui| match self.tab {
                Tab::Live => self.live.ui(ui, &mut self.shared),
                Tab::Sound => self.sound.ui(ui, &mut self.shared),
                Tab::Clips => self.trimmer.ui(ui, &mut self.shared),
                Tab::Settings => self.settings_tab.ui(ui, &mut self.shared),
                Tab::Quality => self.quality.ui(ui, &mut self.shared),
                Tab::Appearance => self.appearance.ui(ui, &mut self.shared, self.theme_error.as_deref()),
            });
        self.dock_preview(&ctx);
        // A 1 px outline around the whole window (no Windows frame anymore).
        let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        if !maximized {
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("window-outline")));
            painter.rect_stroke(ctx.content_rect(), 0.0, border, egui::StrokeKind::Inside);
        }
        self.flush_outbox();
    }
}

/// A small filled circle (recording indicator).
fn status_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 5.0, color);
}

/// Small helpers shared by the tabs.
pub(crate) fn section(ui: &mut egui::Ui, shared: &Shared, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    let c = &shared.theme.colors;
    egui::Frame::new()
        .fill(c.panel.0)
        .stroke(egui::Stroke::new(1.0, c.border.0))
        .corner_radius(egui::CornerRadius::same(shared.theme.corner_radius.clamp(0.0, 255.0) as u8))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).strong().size(shared.theme.fonts.body + 2.0));
            ui.add_space(4.0);
            add(ui);
        });
    ui.add_space(10.0);
}

pub(crate) fn dim(shared: &Shared, text: impl Into<String>) -> RichText {
    RichText::new(text).color(shared.theme.colors.text_dim.0)
}
