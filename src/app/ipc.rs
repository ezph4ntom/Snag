//! Messages between the core process and the UI process.
//!
//! The core starts the UI as a child process with piped stdin/stdout and
//! they exchange one JSON object per line. No sockets, no named pipes, no
//! cleanup: when either side exits, the other sees the pipe close.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Live state the UI shows in its status bar. Sent once per second.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CoreStatus {
    pub recording: bool,
    /// "Recording", "Stopped", "Starting...", "Error: ..."
    pub state: String,
    pub encoder: String,
    pub resolution: String,
    pub fps: u32,
    pub monitor: String,
    pub frames_encoded: u64,
    pub frames_dropped: u64,
    pub buffered_seconds: f64,
    pub last_clip: Option<PathBuf>,
    pub audio: bool,
    /// What the audio track is recorded from right now (device/app names).
    pub audio_sources: Vec<String>,
    pub audio_note: Option<String>,
    /// Notes about the hotkeys, e.g. "intercepted with a keyboard hook".
    pub hotkey_notes: Vec<String>,
    /// Hotkeys that can't work at all, and why.
    pub hotkey_errors: Vec<String>,
    /// A hotkey can't work right now (e.g. an administrator program is
    /// focused).
    pub hotkey_warning: Option<String>,
    /// Some hotkey only works through the keyboard hook.
    pub hook_only_hotkeys: bool,
    /// Snag runs as administrator.
    pub elevated: bool,
    /// Latest notable event ("Saved Clip_... (31 s)", errors).
    pub note: String,
    /// A long recording is running.
    pub rec_active: bool,
    /// Its length so far (seconds) and size (bytes).
    pub rec_seconds: f64,
    pub rec_bytes: u64,
    /// Recording mode is "whole session" (the button saves, not stops).
    pub rec_session_mode: bool,
    /// A recording is being written to the clips folder.
    pub rec_saving: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
#[allow(clippy::large_enum_variant)] // one message a second; boxing buys nothing
pub enum CoreToUi {
    Status(CoreStatus),
    /// Switch to a tab (and load a clip into the trimmer). `activate`:
    /// bring the window to the front; otherwise leave focus alone.
    Show {
        tab: String,
        clip: Option<PathBuf>,
        activate: bool,
    },
    /// Result of `UiToCore::CaptureHotkey`: the combination pressed, or
    /// `None` if cancelled (Esc) or timed out.
    HotkeyCaptured { target: String, hotkey: Option<String> },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum UiToCore {
    /// settings.json was saved; re-read it and apply.
    ReloadSettings,
    ToggleBuffer,
    SaveClip,
    Screenshot,
    /// Record the next key press as a hotkey ("clip" or "screenshot"). The
    /// core does this with a keyboard hook, so any key can be captured.
    CaptureHotkey { target: String },
    CancelHotkeyCapture,
    /// Restart the core as administrator (so hotkeys also work while
    /// programs running as administrator are focused).
    RestartAsAdmin,
    /// The record button: start/stop (manual mode) or save the session.
    Record,
    /// Start/stop sending live preview frames (Live tab shown or not).
    Preview { on: bool },
    /// The Sound choices while a fader is dragged: applied to the recording
    /// right away (settings.json is saved once the drag ends).
    LiveAudio { audio: crate::config::AudioSettings },
}

/// How hotkey registration went, for the Settings tab and the log.
#[derive(Clone, Debug, Default)]
pub struct HotkeyReport {
    pub notes: Vec<String>,
    pub errors: Vec<String>,
    /// Hotkeys Windows wouldn't register (they rely on the keyboard hook).
    pub hook_only: Vec<String>,
}

pub fn encode<T: Serialize>(msg: &T) -> String {
    let mut s = serde_json::to_string(msg).unwrap_or_default();
    s.push('\n');
    s
}
