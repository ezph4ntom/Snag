//! User settings, stored as pretty JSON in `%APPDATA%\Snag\settings.json`.
//!
//! `#[serde(default)]` on every struct means a settings file from an older
//! version (missing fields) still loads: missing fields take their defaults.
//! Unknown fields are ignored, so downgrading also works.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::encode::EncoderKind;
use crate::tune::{HardwareInfo, Preset};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Length of the replay buffer, i.e. how many seconds a clip contains.
    pub buffer_seconds: u32,
    pub start_buffer_on_launch: bool,
    pub clips_dir: PathBuf,
    pub screenshots_dir: PathBuf,
    pub hotkeys: HotkeySettings,
    pub capture: CaptureSettings,
    pub video: VideoSettings,
    pub audio: AudioSettings,
    pub sound: SoundSettings,
    pub trimmer: TrimmerSettings,
    /// Theme id = file name in `themes/` without `.json`.
    pub theme: String,
    /// Window layout: size, tab bar position and style.
    pub ui: UiSettings,
    /// Hardware detected on first launch (cached so startup stays fast).
    pub hardware: Option<HardwareInfo>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            buffer_seconds: 30,
            start_buffer_on_launch: true,
            clips_dir: crate::paths::default_clips_dir(),
            screenshots_dir: crate::paths::default_screenshots_dir(),
            hotkeys: HotkeySettings::default(),
            capture: CaptureSettings::default(),
            video: VideoSettings::default(),
            audio: AudioSettings::default(),
            sound: SoundSettings::default(),
            trimmer: TrimmerSettings::default(),
            theme: "dark".into(),
            ui: UiSettings::default(),
            hardware: None,
        }
    }
}

/// Which window edge the tab bar sits on.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum TabDock {
    Top,
    Left,
    Right,
    Bottom,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct UiSettings {
    /// UI size: 1.0 = 100 %.
    pub scale: f32,
    pub tab_dock: TabDock,
    /// Show tabs as icons only (names in tooltips).
    pub icon_tabs: bool,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self { scale: 1.0, tab_dock: TabDock::Top, icon_tabs: false }
    }
}

impl UiSettings {
    pub const MIN_SCALE: f32 = 0.7;
    pub const MAX_SCALE: f32 = 2.0;
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct HotkeySettings {
    /// Saves the last `buffer_seconds` seconds. Format: "Ctrl+Shift+F10".
    pub clip: String,
    /// Saves a PNG of the captured monitor.
    pub screenshot: String,
}

impl Default for HotkeySettings {
    fn default() -> Self {
        // Ctrl+F10/F11 avoid the NVIDIA overlay (Alt+F9/F10/F1) and Xbox Game
        // Bar (Win+Alt+...) defaults. F12 is avoided entirely: Windows reserves
        // it for debuggers and RegisterHotKey refuses it.
        Self {
            clip: "Ctrl+F10".into(),
            screenshot: "Ctrl+F11".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum CaptureMode {
    /// Capture whichever monitor holds the foreground window.
    FollowFocus,
    /// Always capture `fixed_monitor`.
    Fixed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CaptureSettings {
    pub mode: CaptureMode,
    /// GDI device name like `\\.\DISPLAY2`. Empty = primary monitor.
    pub fixed_monitor: String,
    pub capture_cursor: bool,
}

impl Default for CaptureSettings {
    fn default() -> Self {
        Self {
            mode: CaptureMode::FollowFocus,
            fixed_monitor: String::new(),
            capture_cursor: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct VideoSettings {
    /// Which auto-tuned preset these values came from, or `Custom` after a
    /// manual override.
    pub preset: Preset,
    pub encoder: EncoderKind,
    /// Output height in pixels (width follows the primary monitor's aspect
    /// ratio). 0 = native resolution of the primary monitor.
    pub output_height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

impl Default for VideoSettings {
    fn default() -> Self {
        // Only used until auto-tune runs on first launch.
        Self {
            preset: Preset::Balanced,
            encoder: EncoderKind::X264,
            output_height: 1080,
            fps: 60,
            bitrate_kbps: 12_000,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AudioSettings {
    /// Record PC audio into clips at all.
    pub record_pc_audio: bool,
    /// AAC bitrate. 160 kbps stereo is transparent for game audio.
    pub bitrate_kbps: u32,
    /// true: everything on every playback device. false: only the apps
    /// ticked in `apps`.
    pub whole_pc: bool,
    /// Apps the user ticked or unticked (remembered by exe name, so a
    /// restarted game is picked up again).
    pub apps: Vec<AudioAppChoice>,
    /// Per-device choices for "Whole PC audio" (devices not listed are
    /// included at full volume).
    pub devices: Vec<AudioDeviceChoice>,
    /// Record your microphone (your voice) into clips too.
    pub record_mic: bool,
    /// Microphone endpoint id; empty = Windows' default microphone.
    pub mic_device: String,
    /// 0..=1
    pub mic_volume: f32,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            record_pc_audio: true,
            bitrate_kbps: 160,
            whole_pc: true,
            apps: Vec::new(),
            devices: Vec::new(),
            record_mic: true,
            mic_device: String::new(),
            mic_volume: 1.0,
        }
    }
}

impl AudioSettings {
    /// Changes that need the replay buffer restarted (the rest is applied
    /// live, without losing what's buffered).
    pub fn needs_restart(&self, other: &AudioSettings) -> bool {
        self.record_pc_audio != other.record_pc_audio || self.bitrate_kbps != other.bitrate_kbps
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AudioAppChoice {
    /// Lower-case exe file name, e.g. "spotify.exe".
    pub exe: String,
    /// Display name, e.g. "Spotify".
    pub name: String,
    pub include: bool,
    /// 0..=1
    pub volume: f32,
}

impl Default for AudioAppChoice {
    fn default() -> Self {
        Self { exe: String::new(), name: String::new(), include: true, volume: 1.0 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AudioDeviceChoice {
    /// Windows' endpoint id.
    pub id: String,
    pub name: String,
    pub include: bool,
    /// 0..=1
    pub volume: f32,
}

impl Default for AudioDeviceChoice {
    fn default() -> Self {
        Self { id: String::new(), name: String::new(), include: true, volume: 1.0 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SoundSettings {
    pub enabled: bool,
    /// 0.0 ..= 1.0
    pub volume: f32,
}

impl Default for SoundSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            volume: 0.6,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum AfterClip {
    /// Do nothing (just the sound).
    Nothing,
    /// Load the clip into the trimmer, but start the window minimized so a
    /// fullscreen game keeps focus. This is the default: popping a window in
    /// front of a game mid-match would be worse than useless.
    OpenTrimmerMinimized,
    /// Bring the trimmer to the front immediately.
    OpenTrimmer,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TrimmerSettings {
    pub after_clip: AfterClip,
    /// Default export mode: false = fast (keyframe remux), true = precise.
    pub precise_by_default: bool,
    /// Volume of the preview's sound, 0..=1.
    pub preview_volume: f32,
}

impl Default for TrimmerSettings {
    fn default() -> Self {
        Self {
            after_clip: AfterClip::OpenTrimmerMinimized,
            precise_by_default: false,
            preview_volume: 1.0,
        }
    }
}

impl Settings {
    /// Clamps values a hand-edited file could have broken.
    pub fn sanitize(&mut self) {
        self.buffer_seconds = self.buffer_seconds.clamp(5, 600);
        self.video.fps = self.video.fps.clamp(10, 240);
        self.video.bitrate_kbps = self.video.bitrate_kbps.clamp(500, 200_000);
        if self.video.output_height != 0 {
            self.video.output_height = self.video.output_height.clamp(240, 4320) & !1;
        }
        self.sound.volume = self.sound.volume.clamp(0.0, 1.0);
        self.trimmer.preview_volume = self.trimmer.preview_volume.clamp(0.0, 1.0);
        self.audio.bitrate_kbps = self.audio.bitrate_kbps.clamp(64, 320);
        for a in &mut self.audio.apps {
            a.volume = a.volume.clamp(0.0, 1.0);
            a.exe = a.exe.to_ascii_lowercase();
        }
        self.audio.mic_volume = self.audio.mic_volume.clamp(0.0, 1.0);
        self.ui.scale = (self.ui.scale.clamp(UiSettings::MIN_SCALE, UiSettings::MAX_SCALE) * 10.0).round() / 10.0;
        for d in &mut self.audio.devices {
            d.volume = d.volume.clamp(0.0, 1.0);
        }
    }

    /// Loads settings. Returns `(settings, first_launch)`.
    ///
    /// A corrupt file is moved aside (not deleted) and defaults are used, so
    /// a typo in a hand-edited file can never stop the app from starting.
    pub fn load() -> (Settings, bool) {
        let path = crate::paths::settings_file();
        match read_json_text(&path) {
            Ok(text) => match serde_json::from_str::<Settings>(&text) {
                Ok(mut s) => {
                    s.sanitize();
                    (s, false)
                }
                Err(e) => {
                    log::error!("settings.json is invalid ({e}); using defaults");
                    let _ = std::fs::rename(&path, path.with_extension("corrupt.json"));
                    (Settings::default(), true)
                }
            },
            Err(_) => (Settings::default(), true),
        }
    }

    /// Saves atomically: write a temp file, then rename over the old one, so a
    /// crash mid-write can't leave a half-written settings file.
    pub fn save(&self) -> Result<()> {
        save_json_atomic(&crate::paths::settings_file(), self)
    }

    /// Modification time of the settings file (used to detect external edits).
    pub fn file_mtime() -> Option<std::time::SystemTime> {
        std::fs::metadata(crate::paths::settings_file())
            .and_then(|m| m.modified())
            .ok()
    }
}

/// Reads a JSON file, tolerating a UTF-8 byte-order mark: Windows editors
/// (older Notepad, PowerShell 5) often add one, and serde_json rejects it.
pub fn read_json_text(path: &Path) -> std::io::Result<String> {
    let text = std::fs::read_to_string(path)?;
    Ok(match text.strip_prefix('\u{feff}') {
        Some(rest) => rest.to_string(),
        None => text,
    })
}

pub fn save_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(value)?;
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}
