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
    /// Notes about the hotkeys, e.g. "intercepted with a keyboard hook".
    pub hotkey_notes: Vec<String>,
    /// Latest notable event ("Saved Clip_... (31 s)", errors).
    pub note: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
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
}

pub fn encode<T: Serialize>(msg: &T) -> String {
    let mut s = serde_json::to_string(msg).unwrap_or_default();
    s.push('\n');
    s
}
