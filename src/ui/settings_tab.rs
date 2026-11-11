//! Settings tab: buffer length, hotkeys, capture mode, audio, folders, sound.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use eframe::egui::{self, RichText};

use super::{Shared, dim, section};
use crate::app::ipc::UiToCore;
use crate::config::{AfterClip, CaptureMode};
use crate::hotkeys::Hotkey;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Which {
    Clip,
    Screenshot,
}

impl Which {
    fn target(self) -> &'static str {
        match self {
            Which::Clip => "clip",
            Which::Screenshot => "screenshot",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Folder {
    Clips,
    Screenshots,
}

#[derive(Default)]
pub struct SettingsTab {
    /// Hotkey currently waiting for a key press.
    capturing: Option<Which>,
    /// The core records the key with its keyboard hook (any key works). If
    /// the tray process isn't running, the window records it itself.
    capturing_via_core: bool,
    capture_error: Option<String>,
    /// Info shown after picking a key (e.g. "F9 no longer reaches other apps").
    capture_note: Option<String>,
    picking: Option<(Folder, Receiver<PathBuf>)>,
    monitors: Option<Vec<crate::capture::MonitorInfo>>,
}

/// egui key name -> our hotkey key name (they mostly match already).
fn key_name(key: egui::Key) -> &'static str {
    match key {
        egui::Key::OpenBracket => "LeftBracket",
        egui::Key::CloseBracket => "RightBracket",
        egui::Key::Plus => "Equals",
        k => k.name(),
    }
}

impl SettingsTab {
    pub fn ui(&mut self, ui: &mut egui::Ui, shared: &mut Shared) {
        if let Some((which, rx)) = &self.picking {
            if let Ok(path) = rx.try_recv() {
                match which {
                    Folder::Clips => shared.settings.clips_dir = path,
                    Folder::Screenshots => shared.settings.screenshots_dir = path,
                }
                shared.mark_dirty();
                self.picking = None;
            } else {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            }
        }
        self.listen_for_hotkey(ui, shared);

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(760.0);
            self.buffer_section(ui, shared);
            self.hotkey_section(ui, shared);
            self.capture_section(ui, shared);
            self.audio_section(ui, shared);
            self.folder_section(ui, shared);
            self.sound_section(ui, shared);
            self.after_clip_section(ui, shared);
            ui.label(dim(shared, format!("Settings file: {}", crate::paths::settings_file().display())));
        });
    }

    fn buffer_section(&mut self, ui: &mut egui::Ui, shared: &mut Shared) {
        let s = &mut shared.settings;
        let mut secs = s.buffer_seconds;
        let mut start = s.start_buffer_on_launch;
        section(ui, shared, "Replay buffer", |ui| {
            ui.horizontal(|ui| {
                ui.label("Clip length");
                ui.spacing_mut().slider_width = 320.0;
                ui.add(egui::Slider::new(&mut secs, 5..=300).step_by(5.0).suffix(" s"));
            });
            ui.checkbox(&mut start, "Start the replay buffer when Snag starts");
        });
        let s = &mut shared.settings;
        if secs != s.buffer_seconds || start != s.start_buffer_on_launch {
            s.buffer_seconds = secs;
            s.start_buffer_on_launch = start;
            shared.mark_dirty();
        }
    }

    fn hotkey_section(&mut self, ui: &mut egui::Ui, shared: &mut Shared) {
        let notes = shared.status.as_ref().map(|s| s.hotkey_notes.clone()).unwrap_or_default();
        let clip = shared.settings.hotkeys.clip.clone();
        let shot = shared.settings.hotkeys.screenshot.clone();
        let (error_color, warn_color) = (shared.theme.colors.error.0, shared.theme.colors.warning.0);
        let mut start_capture = None;
        let mut cancel = false;
        let capturing = self.capturing;
        let capture_error = self.capture_error.clone();
        let capture_note = self.capture_note.clone();
        section(ui, shared, "Hotkeys", |ui| {
            egui::Grid::new("hotkeys").num_columns(3).spacing([16.0, 8.0]).show(ui, |ui| {
                for (which, label, value) in [(Which::Clip, "Save clip", &clip), (Which::Screenshot, "Screenshot", &shot)] {
                    ui.label(label);
                    let waiting = capturing == Some(which);
                    let text = if waiting { "Press any key or combination... (Esc cancels)".to_string() } else { value.clone() };
                    if ui.add(egui::Button::new(RichText::new(text).monospace()).min_size([300.0, 0.0].into())).clicked() && !waiting {
                        start_capture = Some(which);
                    }
                    if waiting && ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                    ui.end_row();
                }
            });
            ui.label(
                RichText::new(
                    "Click a hotkey, then press any key: F-keys, letters, Print Screen, a lone Right Ctrl, \
                     with or without Ctrl/Alt/Shift/Win. Snag takes the key over even if Windows or \
                     another program uses it. Picking the other action's key swaps the two.",
                )
                .small(),
            );
            if let Some(n) = &capture_note {
                ui.label(RichText::new(n).color(warn_color).small());
            }
            for n in &notes {
                ui.label(RichText::new(n).small());
            }
            if let Some(e) = &capture_error {
                ui.label(RichText::new(e).color(error_color));
            }
        });
        if let Some(w) = start_capture {
            // A capture already running for the other hotkey is replaced.
            if self.capturing_via_core {
                shared.outbox.push(UiToCore::CancelHotkeyCapture);
            }
            self.capturing = Some(w);
            self.capture_error = None;
            self.capture_note = None;
            self.capturing_via_core = shared.connected;
            if self.capturing_via_core {
                shared.outbox.push(UiToCore::CaptureHotkey { target: w.target().into() });
            }
        }
        if cancel {
            if self.capturing_via_core {
                shared.outbox.push(UiToCore::CancelHotkeyCapture);
            }
            self.capturing = None;
        }
    }

    /// Applies a newly picked hotkey. If it's the other action's hotkey, the
    /// two swap (so neither ends up without a key).
    fn apply_hotkey(&mut self, which: Which, hk: Hotkey, shared: &mut Shared) {
        let text = hk.to_string();
        let keys = &mut shared.settings.hotkeys;
        let (mine, other) = match which {
            Which::Clip => (&mut keys.clip, &mut keys.screenshot),
            Which::Screenshot => (&mut keys.screenshot, &mut keys.clip),
        };
        if Hotkey::parse(other).ok() == Some(hk) {
            *other = mine.clone();
            self.capture_note = Some(format!("{text} was the other hotkey; the two were swapped."));
        } else if hk.is_plain_key() {
            self.capture_note = Some(format!("{text} will only trigger Snag now; it no longer reaches games or other apps while Snag runs."));
        } else if hk.is_modifier_only() {
            self.capture_note = Some(format!("{text} keeps working normally too; pressing it alone triggers Snag."));
        }
        *mine = text;
        shared.mark_dirty();
    }

    /// Picks up the result of a key capture: from the core (any key), or by
    /// watching this window's own key events when the core isn't running.
    fn listen_for_hotkey(&mut self, ui: &mut egui::Ui, shared: &mut Shared) {
        let Some(which) = self.capturing else { return };
        if self.capturing_via_core {
            if let Some((target, result)) = shared.captured_hotkey.take() {
                if target != which.target() {
                    return; // stale answer for a capture we replaced
                }
                self.capturing = None;
                match result.as_deref().map(Hotkey::parse) {
                    Some(Ok(hk)) => self.apply_hotkey(which, hk, shared),
                    Some(Err(e)) => self.capture_error = Some(format!("{e:#}")),
                    None => {} // Esc / timeout
                }
            } else if !shared.connected {
                self.capturing = None; // the tray process went away
            }
            return;
        }
        let pressed = ui.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Key { key, pressed: true, modifiers, .. } => Some((*key, *modifiers)),
                _ => None,
            })
        });
        let Some((key, mods)) = pressed else { return };
        self.capturing = None;
        if key == egui::Key::Escape {
            return;
        }
        match Hotkey::from_parts(mods.ctrl, mods.alt, mods.shift, false, key_name(key)) {
            Some(hk) => self.apply_hotkey(which, hk, shared),
            None => self.capture_error = Some(format!("{} can't be recorded here; start Snag (the tray app) to record any key", key.name())),
        }
    }

    fn audio_section(&mut self, ui: &mut egui::Ui, shared: &mut Shared) {
        let mut a = shared.settings.audio.clone();
        section(ui, shared, "Audio", |ui| {
            ui.checkbox(&mut a.record_pc_audio, "Record PC audio (everything you hear: game, voice chat, music)");
            ui.add_enabled_ui(a.record_pc_audio, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Quality");
                    egui::ComboBox::from_id_salt("audio_kbps")
                        .selected_text(format!("{} kbps AAC", a.bitrate_kbps))
                        .show_ui(ui, |ui| {
                            for k in [128, 160, 192, 256, 320] {
                                ui.selectable_value(&mut a.bitrate_kbps, k, format!("{k} kbps AAC"));
                            }
                        });
                });
            });
            ui.label(
                RichText::new(
                    "Records the default playback device and follows it if you switch (speakers to headset). \
                     Changing this restarts the replay buffer.",
                )
                .small(),
            );
        });
        if a != shared.settings.audio {
            shared.settings.audio = a;
            shared.mark_dirty();
        }
    }

    fn capture_section(&mut self, ui: &mut egui::Ui, shared: &mut Shared) {
        let monitors = self.monitors.get_or_insert_with(crate::capture::enumerate_monitors).clone();
        let mut cap = shared.settings.capture.clone();
        section(ui, shared, "Capture", |ui| {
            ui.radio_value(&mut cap.mode, CaptureMode::FollowFocus, "Follow focus: capture the monitor with the active window");
            ui.label(RichText::new("    Alt-Tab to a window on another monitor and capture moves with you (after 0.2 s).").small());
            ui.horizontal(|ui| {
                ui.radio_value(&mut cap.mode, CaptureMode::Fixed, "Always capture");
                let current = monitors
                    .iter()
                    .find(|m| m.device_name == cap.fixed_monitor)
                    .or_else(|| monitors.iter().find(|m| m.primary))
                    .map(|m| m.label())
                    .unwrap_or_else(|| "primary monitor".into());
                ui.add_enabled_ui(cap.mode == CaptureMode::Fixed, |ui| {
                    egui::ComboBox::from_id_salt("fixed_monitor").selected_text(current).width(320.0).show_ui(ui, |ui| {
                        for m in &monitors {
                            ui.selectable_value(&mut cap.fixed_monitor, m.device_name.clone(), m.label());
                        }
                    });
                });
            });
            ui.checkbox(&mut cap.capture_cursor, "Show the mouse cursor in clips");
        });
        if cap != shared.settings.capture {
            shared.settings.capture = cap;
            shared.mark_dirty();
        }
    }

    fn folder_section(&mut self, ui: &mut egui::Ui, shared: &mut Shared) {
        let mut pick = None;
        let rows = [
            (Folder::Clips, "Clips", shared.settings.clips_dir.clone()),
            (Folder::Screenshots, "Screenshots", shared.settings.screenshots_dir.clone()),
        ];
        let mut edited: Vec<(Folder, PathBuf)> = Vec::new();
        section(ui, shared, "Folders", |ui| {
            egui::Grid::new("folders").num_columns(3).spacing([12.0, 8.0]).show(ui, |ui| {
                for (which, label, path) in &rows {
                    ui.label(*label);
                    let mut text = path.display().to_string();
                    if ui.add_sized([420.0, 22.0], egui::TextEdit::singleline(&mut text)).changed() {
                        edited.push((*which, PathBuf::from(text)));
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Browse...").clicked() {
                            pick = Some((*which, path.clone()));
                        }
                        if ui.button("Open").clicked() {
                            crate::win::reveal_in_explorer(path);
                        }
                    });
                    ui.end_row();
                }
            });
        });
        for (which, path) in edited {
            match which {
                Folder::Clips => shared.settings.clips_dir = path,
                Folder::Screenshots => shared.settings.screenshots_dir = path,
            }
            shared.mark_dirty();
        }
        if let Some((which, path)) = pick {
            self.picking = Some((which, super::dialogs::pick_folder(&path)));
        }
    }

    fn sound_section(&mut self, ui: &mut egui::Ui, shared: &mut Shared) {
        let mut snd = shared.settings.sound.clone();
        let file = crate::paths::clip_sound_file();
        let exists = file.is_file();
        let warn = shared.theme.colors.warning.0;
        let mut test = false;
        section(ui, shared, "Clip sound", |ui| {
            ui.checkbox(&mut snd.enabled, "Play a sound when a clip is saved");
            ui.add_enabled_ui(snd.enabled, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Volume");
                    ui.spacing_mut().slider_width = 220.0;
                    let mut pct = (snd.volume * 100.0).round() as u32;
                    if ui.add(egui::Slider::new(&mut pct, 0..=100).suffix(" %")).changed() {
                        snd.volume = pct as f32 / 100.0;
                    }
                    test = ui.button("Test").clicked();
                });
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("Sound file: {}", file.display())).small());
                if ui.small_button("Show").clicked() {
                    crate::win::reveal_in_explorer(if exists { &file } else { file.parent().unwrap_or(&file) });
                }
            });
            ui.label(RichText::new("Replace clip.wav with any WAV file to use your own sound.").small());
            if !exists {
                ui.label(RichText::new("clip.wav not found; the built-in sound is used.").color(warn).small());
            }
        });
        if test {
            crate::sound::play_clip_sound(snd.volume);
        }
        if snd != shared.settings.sound {
            shared.settings.sound = snd;
            shared.mark_dirty();
        }
    }

    fn after_clip_section(&mut self, ui: &mut egui::Ui, shared: &mut Shared) {
        let mut t = shared.settings.trimmer.clone();
        section(ui, shared, "After saving a clip", |ui| {
            ui.radio_value(&mut t.after_clip, AfterClip::OpenTrimmerMinimized, "Open it in the trimmer, minimized (the game keeps focus)");
            ui.radio_value(&mut t.after_clip, AfterClip::OpenTrimmer, "Open it in the trimmer in front");
            ui.radio_value(&mut t.after_clip, AfterClip::Nothing, "Do nothing (just the sound)");
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("Default trim mode:");
                ui.radio_value(&mut t.precise_by_default, false, "Fast (lossless)");
                ui.radio_value(&mut t.precise_by_default, true, "Precise (re-encode)");
            });
        });
        if t != shared.settings.trimmer {
            shared.settings.trimmer = t;
            shared.mark_dirty();
        }
    }
}
