//! System tray icon and its menu. Lives on the main thread (tray-icon needs
//! the thread that created it to run a Win32 message loop, which ours does).

use std::sync::mpsc::Sender;

use anyhow::Result;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use super::controller::{Event, TrayAction};

pub struct Tray {
    icon: TrayIcon,
    toggle: MenuItem,
    save: MenuItem,
    shot: MenuItem,
    record: MenuItem,
    /// (recording, light taskbar) -> icon, built lazily.
    icons: std::cell::RefCell<std::collections::HashMap<(bool, bool), Icon>>,
}

/// The Snag glyph for the tray: light on a dark taskbar, dark on a light
/// one (Windows "light mode"), sized to the system's small-icon size. The
/// dot is Ember while recording and grey when stopped.
fn make_icon(recording: bool, light_taskbar: bool) -> Icon {
    use crate::brand;
    // SAFETY: plain metric query (16 px at 100% scaling, 32 at 200%).
    let size = unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(windows::Win32::UI::WindowsAndMessaging::SM_CXSMICON)
    }
    .clamp(16, 64) as u32;
    let body = if light_taskbar { brand::DARK_GLYPH } else { brand::LIGHT };
    let dot = match (recording, light_taskbar) {
        (true, _) => brand::EMBER,
        (false, true) => brand::MUTED_ON_LIGHT,
        (false, false) => brand::MUTED,
    };
    Icon::from_rgba(brand::glyph_rgba(size, body, dot), size, size).expect("valid icon")
}

const ID_SAVE: &str = "save_clip";
const ID_SHOT: &str = "screenshot";
const ID_TOGGLE: &str = "toggle_buffer";
const ID_RECORD: &str = "record";
const ID_CLIPS: &str = "open_clips";
const ID_TRIM: &str = "open_trimmer";
const ID_SETTINGS: &str = "settings";
const ID_QUIT: &str = "quit";

impl Tray {
    pub fn new(events: Sender<Event>) -> Result<Tray> {
        let save = MenuItem::with_id(ID_SAVE, "Save clip", true, None);
        let shot = MenuItem::with_id(ID_SHOT, "Take screenshot", true, None);
        let toggle = MenuItem::with_id(ID_TOGGLE, "Start replay buffer", true, None);
        let record = MenuItem::with_id(ID_RECORD, "Start recording", true, None);
        let menu = Menu::with_items(&[
            &save,
            &shot,
            &record,
            &PredefinedMenuItem::separator(),
            &toggle,
            &MenuItem::with_id(ID_CLIPS, "Open clips folder", true, None),
            &MenuItem::with_id(ID_TRIM, "Clips && trimmer...", true, None),
            &MenuItem::with_id(ID_SETTINGS, "Settings...", true, None),
            &PredefinedMenuItem::separator(),
            &MenuItem::with_id(ID_QUIT, "Quit Snag", true, None),
        ])?;

        // Menu clicks arrive on this (main) thread via the tray's window
        // procedure; forward them to the controller thread.
        let tx = events.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let action = match e.id.0.as_str() {
                ID_SAVE => TrayAction::SaveClip,
                ID_SHOT => TrayAction::Screenshot,
                ID_TOGGLE => TrayAction::ToggleBuffer,
                ID_RECORD => TrayAction::Record,
                ID_CLIPS => TrayAction::OpenClipsFolder,
                ID_TRIM => TrayAction::OpenTrimmer,
                ID_SETTINGS => TrayAction::OpenSettings,
                ID_QUIT => TrayAction::Quit,
                _ => return,
            };
            let _ = tx.send(Event::Tray(action));
        }));
        // Left-click on the icon opens the window; right-click shows the menu.
        let tx = events;
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = e
            {
                let _ = tx.send(Event::Tray(TrayAction::OpenTrimmer));
            }
        }));

        let idle_icon = make_icon(false, crate::win::taskbar_is_light());
        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("Snag")
            .with_icon(idle_icon.clone())
            .build()?;
        Ok(Tray {
            icon,
            toggle,
            save,
            shot,
            record,
            icons: Default::default(),
        })
    }

    /// Updates icon, tooltip and menu labels.
    pub fn update(&self, recording: bool, tooltip: &str, clip_key: &str, shot_key: &str, record_label: &str) {
        self.record.set_text(record_label);
        let light = crate::win::taskbar_is_light();
        let icon = self.icons.borrow_mut().entry((recording, light)).or_insert_with(|| make_icon(recording, light)).clone();
        let _ = self.icon.set_icon(Some(icon));
        // Windows limits tray tooltips to 127 characters.
        let tip: String = tooltip.chars().take(127).collect();
        let _ = self.icon.set_tooltip(Some(tip));
        self.toggle.set_text(if recording { "Stop replay buffer" } else { "Start replay buffer" });
        self.save.set_text(format!("Save clip\t{clip_key}"));
        self.save.set_enabled(recording);
        self.shot.set_text(format!("Take screenshot\t{shot_key}"));
    }
}
