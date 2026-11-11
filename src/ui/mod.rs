//! The window: Clips (trimmer), Settings, Quality and Appearance tabs.
//!
//! Runs as its own process (`snag.exe --ui`), started by the core. It
//! reads `CoreStatus` lines on stdin and writes `UiToCore` commands on stdout
//! (see `app::ipc`). Settings are saved straight to settings.json and the
//! core is told to reload them.

mod appearance_tab;
mod dialogs;
mod quality_tab;
mod settings_tab;
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
    Clips,
    Settings,
    Quality,
    Appearance,
}

impl Tab {
    fn parse(s: &str) -> Tab {
        match s {
            "settings" => Tab::Settings,
            "quality" => Tab::Quality,
            "appearance" | "themes" => Tab::Appearance,
            _ => Tab::Clips,
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
    quality: quality_tab::QualityTab,
    appearance: appearance_tab::AppearanceTab,
    /// Window to minimize (without focus) once eframe has shown it.
    minimize_pending: Option<isize>,
    /// The header logo, drawn in the theme's text and accent colors.
    logo: Option<(egui::TextureHandle, (Color32, Color32))>,
}

pub fn run(args: UiArgs) -> Result<()> {
    crate::logging::init("ui");
    crate::paths::migrate_from_clipapp();
    let (settings, _) = Settings::load();
    let tab = Tab::parse(args.tab.as_deref().unwrap_or("clips"));

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Snag")
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
                quality: quality_tab::QualityTab::default(),
                appearance: appearance_tab::AppearanceTab::default(),
                minimize_pending: if minimized { hwnd } else { None },
                logo: None,
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

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        let c = self.shared.theme.colors.clone();
        // Redraw the logo when the theme's colors change: the glyph takes the
        // text color and the dot follows the accent, as on the brand board.
        let key = (c.text.0, c.accent.0);
        if self.logo.as_ref().is_none_or(|(_, k)| *k != key) {
            let rgb = |c: Color32| [c.r(), c.g(), c.b()];
            let px = crate::brand::glyph_rgba(64, rgb(c.text.0), rgb(c.accent.0));
            let image = egui::ColorImage::from_rgba_unmultiplied([64, 64], &px);
            let tex = ui.ctx().load_texture("snag-logo", image, egui::TextureOptions::LINEAR);
            self.logo = Some((tex, key));
        }
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            if let Some((tex, _)) = &self.logo {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::hover());
                let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                ui.painter().image(tex.id(), rect, uv, Color32::WHITE);
            }
            // The wordmark: lowercase, bold, tightly tracked (-0.04 em).
            ui.label(
                RichText::new("snag")
                    .family(crate::themes::brand_bold_family())
                    .size(21.0)
                    .extra_letter_spacing(-0.8)
                    .color(c.text.0),
            );
            ui.add_space(14.0);
            for (tab, label) in [
                (Tab::Clips, "Clips & trimmer"),
                (Tab::Settings, "Settings"),
                (Tab::Quality, "Quality"),
                (Tab::Appearance, "Appearance"),
            ] {
                let selected = self.tab == tab;
                let text = if selected {
                    RichText::new(label).color(self.shared.theme.colors.accent_text.0)
                } else {
                    RichText::new(label)
                };
                let button = egui::Button::new(text).fill(if selected {
                    self.shared.theme.colors.accent.0
                } else {
                    Color32::TRANSPARENT
                });
                if ui.add(button).clicked() {
                    self.tab = tab;
                }
            }
        });
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
        // Wake up for debounced saves and the theme/clip-list checks.
        ctx.request_repaint_after(Duration::from_millis(if self.shared.dirty_at.is_some() { 100 } else { 1000 }));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let panel = self.shared.theme.colors.panel.0;
        let margin = egui::Margin::symmetric(8, 6);
        egui::Panel::top("tabs")
            .frame(egui::Frame::new().fill(panel).inner_margin(margin))
            .show(ui, |ui| self.tab_bar(ui));
        egui::Panel::bottom("status")
            .frame(egui::Frame::new().fill(panel).inner_margin(margin))
            .show(ui, |ui| self.status_bar(ui));
        let bg = self.shared.theme.colors.background.0;
        let m = self.shared.theme.spacing.window_margin.clamp(0.0, 127.0) as i8;
        egui::CentralPanel::default_margins()
            .frame(egui::Frame::new().fill(bg).inner_margin(egui::Margin::same(m)))
            .show(ui, |ui| match self.tab {
                Tab::Clips => self.trimmer.ui(ui, &mut self.shared),
                Tab::Settings => self.settings_tab.ui(ui, &mut self.shared),
                Tab::Quality => self.quality.ui(ui, &mut self.shared),
                Tab::Appearance => self.appearance.ui(ui, &mut self.shared, self.theme_error.as_deref()),
            });
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
