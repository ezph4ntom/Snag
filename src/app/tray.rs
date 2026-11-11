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
    idle_icon: Icon,
    rec_icon: Icon,
}

/// Draws the tray icon procedurally (no image files to ship): a dark
/// rounded square with a dot, red while recording, gray when stopped.
fn make_icon(recording: bool) -> Icon {
    const S: usize = 32;
    let mut rgba = vec![0u8; S * S * 4];
    let dot = if recording { [235, 64, 52] } else { [150, 150, 150] };
    for y in 0..S {
        for x in 0..S {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            // Rounded square: distance outside a 22x22 box with 5 px corners.
            let qx = ((fx - 16.0).abs() - 11.0).max(0.0);
            let qy = ((fy - 16.0).abs() - 11.0).max(0.0);
            let sq = (qx * qx + qy * qy).sqrt() - 5.0;
            let bg_a = (0.5 - sq).clamp(0.0, 1.0);
            // Dot of radius 7 in the middle, anti-aliased edge.
            let d = ((fx - 16.0).powi(2) + (fy - 16.0).powi(2)).sqrt() - 7.0;
            let dot_a = (0.5 - d).clamp(0.0, 1.0);
            let base = [32.0, 34.0, 40.0];
            let px = &mut rgba[(y * S + x) * 4..][..4];
            for c in 0..3 {
                px[c] = (base[c] * (1.0 - dot_a) + dot[c] as f32 * dot_a) as u8;
            }
            px[3] = (bg_a.max(dot_a) * 255.0) as u8;
        }
    }
    Icon::from_rgba(rgba, S as u32, S as u32).expect("valid icon")
}

const ID_SAVE: &str = "save_clip";
const ID_SHOT: &str = "screenshot";
const ID_TOGGLE: &str = "toggle_buffer";
const ID_CLIPS: &str = "open_clips";
const ID_TRIM: &str = "open_trimmer";
const ID_SETTINGS: &str = "settings";
const ID_QUIT: &str = "quit";

impl Tray {
    pub fn new(events: Sender<Event>) -> Result<Tray> {
        let save = MenuItem::with_id(ID_SAVE, "Save clip", true, None);
        let shot = MenuItem::with_id(ID_SHOT, "Take screenshot", true, None);
        let toggle = MenuItem::with_id(ID_TOGGLE, "Start replay buffer", true, None);
        let menu = Menu::with_items(&[
            &save,
            &shot,
            &PredefinedMenuItem::separator(),
            &toggle,
            &MenuItem::with_id(ID_CLIPS, "Open clips folder", true, None),
            &MenuItem::with_id(ID_TRIM, "Clips && trimmer...", true, None),
            &MenuItem::with_id(ID_SETTINGS, "Settings...", true, None),
            &PredefinedMenuItem::separator(),
            &MenuItem::with_id(ID_QUIT, "Quit ClipApp", true, None),
        ])?;

        // Menu clicks arrive on this (main) thread via the tray's window
        // procedure; forward them to the controller thread.
        let tx = events.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let action = match e.id.0.as_str() {
                ID_SAVE => TrayAction::SaveClip,
                ID_SHOT => TrayAction::Screenshot,
                ID_TOGGLE => TrayAction::ToggleBuffer,
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

        let idle_icon = make_icon(false);
        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("ClipApp")
            .with_icon(idle_icon.clone())
            .build()?;
        Ok(Tray {
            icon,
            toggle,
            save,
            shot,
            idle_icon,
            rec_icon: make_icon(true),
        })
    }

    /// Updates icon, tooltip and menu labels.
    pub fn update(&self, recording: bool, tooltip: &str, clip_key: &str, shot_key: &str) {
        let _ = self.icon.set_icon(Some(if recording { self.rec_icon.clone() } else { self.idle_icon.clone() }));
        // Windows limits tray tooltips to 127 characters.
        let tip: String = tooltip.chars().take(127).collect();
        let _ = self.icon.set_tooltip(Some(tip));
        self.toggle.set_text(if recording { "Stop replay buffer" } else { "Start replay buffer" });
        self.save.set_text(format!("Save clip\t{clip_key}"));
        self.save.set_enabled(recording);
        self.shot.set_text(format!("Take screenshot\t{shot_key}"));
    }
}
