//! The controller thread: owns the settings and the recorder, reacts to
//! hotkeys/tray/focus/UI events. Everything that can take a while (saving a
//! clip, writing a PNG, detecting hardware) runs on a worker thread so the
//! controller stays responsive.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::ipc::{CoreStatus, CoreToUi, UiToCore};
use super::{MainRequest, post_to_main};
use crate::buffer::clip::{ClipResult, save_clip};
use crate::buffer::source::{FrameSource, WgcSource};
use crate::buffer::{Recorder, RecorderConfig, Session, SourceFactory};
use crate::config::{AfterClip, CaptureMode, Settings};

#[derive(Debug, Clone, Copy)]
pub enum TrayAction {
    SaveClip,
    Screenshot,
    ToggleBuffer,
    OpenClipsFolder,
    OpenTrimmer,
    OpenSettings,
    Quit,
}

pub enum Event {
    Hotkey(i32),
    Tray(TrayAction),
    /// The foreground window changed (monitor it's on, owning process id,
    /// and window class for filtering shell transients).
    Foreground { monitor: isize, pid: u32, class: String },
    /// Per-hotkey notes from registration (e.g. "caught with the hook").
    HotkeyNotes(Vec<String>),
    /// The key press recorded for a hotkey being rebound (None = cancelled).
    HotkeyCaptured { target: String, hotkey: Option<String> },
    ClipDone(Result<ClipResult, String>),
    ScreenshotDone(Result<PathBuf, String>),
    FromUi(UiToCore),
    /// A second `clipapp.exe` was started: show the window.
    ShowUi,
}

struct SaveJob {
    session: Arc<Session>,
    pressed_at: Instant,
    seconds: f64,
    out_dir: PathBuf,
}

struct UiProcess {
    child: Child,
    stdin: ChildStdin,
}

pub struct Controller {
    settings: Settings,
    first_launch: bool,
    events: Sender<Event>,
    recorder: Option<Recorder>,
    saver: Sender<SaveJob>,
    ui: Option<UiProcess>,
    session_counter: u32,
    /// Monitor of the foreground window (for follow-focus mode).
    focused_monitor: isize,
    /// Debounced focus change: (monitor, when it was first seen).
    pending_switch: Option<(isize, Instant)>,
    hotkey_notes: Vec<String>,
    /// When a "press the new hotkey" capture started (it times out).
    capture_started: Option<Instant>,
    last_clip: Option<PathBuf>,
    note: String,
    /// Auto-restart after a recorder failure, with backoff.
    restart_at: Option<Instant>,
    restart_attempts: u32,
    last_status: Instant,
    settings_mtime: Option<std::time::SystemTime>,
}

impl Controller {
    pub fn new(settings: Settings, first_launch: bool, events: Sender<Event>) -> Controller {
        // One saver thread processes clip saves in order. Each job carries the
        // instant the key was pressed, so queueing never shifts a clip.
        let (saver, jobs) = mpsc::channel::<SaveJob>();
        let ev = events.clone();
        std::thread::Builder::new()
            .name("clip-saver".into())
            .spawn(move || {
                for job in jobs {
                    let result = save_clip(&job.session, job.pressed_at, job.seconds, &job.out_dir)
                        .map_err(|e| format!("{e:#}"));
                    if ev.send(Event::ClipDone(result)).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn saver");
        Controller {
            settings,
            first_launch,
            events,
            recorder: None,
            saver,
            ui: None,
            session_counter: 0,
            focused_monitor: 0,
            pending_switch: None,
            hotkey_notes: Vec::new(),
            capture_started: None,
            last_clip: None,
            note: String::new(),
            restart_at: None,
            restart_attempts: 0,
            last_status: Instant::now(),
            settings_mtime: Settings::file_mtime(),
        }
    }

    pub fn run(mut self, rx: Receiver<Event>) {
        self.startup();
        loop {
            let timeout = if self.pending_switch.is_some() {
                Duration::from_millis(50)
            } else {
                Duration::from_millis(500)
            };
            match rx.recv_timeout(timeout) {
                Ok(Event::Tray(TrayAction::Quit)) => break,
                Ok(ev) => self.handle(ev),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.housekeeping();
        }
        self.shutdown();
    }

    fn startup(&mut self) {
        // Detect and auto-tune on first launch, and re-detect whenever the
        // saved results don't describe this PC any more: a different GPU
        // (app moved to another PC, new graphics card), an older detection
        // format, or no working encoder last time (ffmpeg was missing).
        let gpus_now: Vec<String> = crate::tune::detect_gpus().into_iter().map(|g| g.name).collect();
        // Results that don't fit this PC at all: start over with a fresh
        // recommendation. An older detection format alone keeps the preset.
        let different_pc = self.settings.hardware.as_ref().is_none_or(|h| {
            h.encoders.is_empty() || h.gpus.iter().map(|g| g.name.clone()).collect::<Vec<_>>() != gpus_now
        });
        let stale = different_pc || self.settings.hardware.as_ref().is_none_or(|h| !h.is_current());
        if stale && !self.first_launch {
            log::info!("hardware changed or detection incomplete; re-detecting");
        }
        if self.first_launch || stale {
            self.note = "Detecting hardware...".into();
            self.update_tray();
            let hw = crate::tune::detect();
            let fresh = self.first_launch || different_pc;
            // Re-detection after an upgrade keeps the user's choices; a new
            // PC/GPU (or a preset still on auto) gets re-tuned.
            if fresh || self.settings.video.preset != crate::tune::Preset::Custom {
                let rec = crate::tune::recommend(&hw);
                let preset = if fresh { rec.preset } else { self.settings.video.preset };
                let plan = crate::tune::plan(&hw, preset);
                plan.apply_to(&mut self.settings.video);
                log::info!("auto-tune: {} ({:?}). Recommended because: {}", preset.label(), plan, rec.why.join(" "));
            }
            self.settings.hardware = Some(hw);
            self.save_settings();
        }
        // The configured encoder may have disappeared (GPU swap, driver update).
        if let Some(hw) = &self.settings.hardware {
            if !hw.encoders.contains(&self.settings.video.encoder) && !hw.encoders.is_empty() {
                log::warn!(
                    "configured encoder {:?} isn't available; using {:?}",
                    self.settings.video.encoder,
                    hw.encoders[0]
                );
                self.settings.video.encoder = hw.encoders[0];
            }
        }
        if let Err(e) = crate::encode::detect::ffmpeg_version() {
            self.note = "ffmpeg is missing: install it with  winget install Gyan.FFmpeg  and restart ClipApp".into();
            log::error!("ffmpeg not found: {e:#}");
            // Tell the user how to fix it (on a separate thread: the dialog
            // blocks until closed).
            std::thread::spawn(|| {
                crate::win::error_box(
                    "ClipApp needs ffmpeg to record, and it isn't installed.\n\n\
                     To install it, open PowerShell (or Terminal) and run:\n\n\
                         winget install Gyan.FFmpeg\n\n\
                     Then quit ClipApp (right-click the tray icon > Quit) and start it again.\n\
                     (Or put ffmpeg.exe and ffprobe.exe in a \"bin\" folder next to clipapp.exe.)",
                );
            });
        }
        self.focused_monitor = foreground_monitor();
        self.register_hotkeys();
        if self.settings.start_buffer_on_launch {
            self.start_buffer();
        }
        self.update_tray();
        if self.first_launch {
            // Show what auto-tune chose and why.
            self.open_ui("quality", None, true, false);
        }
    }

    fn handle(&mut self, ev: Event) {
        match ev {
            Event::Hotkey(crate::hotkeys::ID_CLIP) | Event::Tray(TrayAction::SaveClip) | Event::FromUi(UiToCore::SaveClip) => {
                self.save_clip()
            }
            Event::Hotkey(crate::hotkeys::ID_SCREENSHOT)
            | Event::Tray(TrayAction::Screenshot)
            | Event::FromUi(UiToCore::Screenshot) => self.screenshot(),
            Event::Hotkey(_) => {}
            Event::Tray(TrayAction::ToggleBuffer) | Event::FromUi(UiToCore::ToggleBuffer) => {
                if self.recorder.is_some() {
                    self.stop_buffer();
                    self.note = "Replay buffer stopped".into();
                } else {
                    self.restart_attempts = 0;
                    self.start_buffer();
                }
                self.update_tray();
            }
            Event::Tray(TrayAction::OpenClipsFolder) => crate::win::reveal_in_explorer(&self.settings.clips_dir),
            Event::Tray(TrayAction::OpenTrimmer) | Event::ShowUi => {
                let clip = self.last_clip.clone();
                self.open_ui("clips", clip.as_deref(), true, false)
            }
            Event::Tray(TrayAction::OpenSettings) => self.open_ui("settings", None, true, false),
            Event::Tray(TrayAction::Quit) => {}
            Event::Foreground { monitor, pid, class } => self.on_foreground(monitor, pid, &class),
            Event::HotkeyNotes(notes) => {
                for n in &notes {
                    log::info!("hotkey: {n}");
                }
                self.hotkey_notes = notes;
            }
            Event::FromUi(UiToCore::CaptureHotkey { target }) => {
                self.capture_started = Some(Instant::now());
                post_to_main(MainRequest::BeginHotkeyCapture { target });
            }
            Event::FromUi(UiToCore::CancelHotkeyCapture) => post_to_main(MainRequest::CancelHotkeyCapture),
            Event::HotkeyCaptured { target, hotkey } => {
                self.capture_started = None;
                let msg = CoreToUi::HotkeyCaptured { target, hotkey };
                self.send_to_ui(&super::ipc::encode(&msg));
            }
            Event::ClipDone(Ok(r)) => {
                log::info!(
                    "saved {} ({:.1} s, {} segments, {:.1} MB) in {} ms{}",
                    r.path.display(),
                    r.seconds,
                    r.segments,
                    r.bytes as f64 / 1e6,
                    r.took.as_millis(),
                    if r.warnings.is_empty() { String::new() } else { format!("; {}", r.warnings.join("; ")) }
                );
                if self.settings.sound.enabled {
                    crate::sound::play_clip_sound(self.settings.sound.volume);
                }
                self.note = format!(
                    "Saved {} ({:.0} s)",
                    r.path.file_name().unwrap_or_default().to_string_lossy(),
                    r.seconds
                );
                self.last_clip = Some(r.path.clone());
                match self.settings.trimmer.after_clip {
                    AfterClip::Nothing => {}
                    AfterClip::OpenTrimmerMinimized => self.open_ui("clips", Some(&r.path), false, true),
                    AfterClip::OpenTrimmer => self.open_ui("clips", Some(&r.path), true, false),
                }
                self.update_tray();
            }
            Event::ClipDone(Err(e)) => {
                log::error!("saving clip failed: {e}");
                self.note = format!("Clip failed: {e}");
                error_beep();
                self.update_tray();
            }
            Event::ScreenshotDone(Ok(path)) => {
                log::info!("screenshot saved: {}", path.display());
                self.note = format!("Screenshot {}", path.file_name().unwrap_or_default().to_string_lossy());
            }
            Event::ScreenshotDone(Err(e)) => {
                log::error!("screenshot failed: {e}");
                self.note = format!("Screenshot failed: {e}");
                error_beep();
            }
            Event::FromUi(UiToCore::ReloadSettings) => self.reload_settings(),
        }
    }

    /// Runs after every event and at least twice a second.
    fn housekeeping(&mut self) {
        // Nobody pressed anything for the new hotkey: give up.
        if self.capture_started.is_some_and(|t| t.elapsed() > Duration::from_secs(20)) {
            self.capture_started = None;
            post_to_main(MainRequest::CancelHotkeyCapture);
        }
        // Apply a focus change once it has been stable for 200 ms, so
        // Alt+Tab flicking through windows doesn't thrash the capture.
        if let Some((mon, since)) = self.pending_switch {
            if since.elapsed() >= Duration::from_millis(200) {
                self.pending_switch = None;
                if let Some(rec) = &self.recorder {
                    if rec.session.monitor.load(Ordering::Relaxed) != mon {
                        log::info!("focus moved: switching capture to {}", monitor_name(mon));
                        rec.switch_monitor(mon);
                    }
                }
            }
        }

        // Recorder died (ffmpeg crashed, GPU reset...)? Restart with backoff.
        let failed = self.recorder.as_ref().and_then(|r| (!r.session.is_running()).then(|| r.session.error()));
        if let Some(err) = failed {
            let err = err.unwrap_or_else(|| "stopped unexpectedly".into());
            self.stop_buffer();
            self.restart_attempts += 1;
            if self.restart_attempts <= 5 {
                let delay = Duration::from_secs(2u64.pow(self.restart_attempts.min(4)));
                self.note = format!("Recording error: {err}. Restarting in {} s", delay.as_secs());
                self.restart_at = Some(Instant::now() + delay);
            } else {
                self.note = format!("Recording stopped after repeated errors: {err}");
            }
            log::error!("{}", self.note);
            self.update_tray();
        }
        if self.restart_at.is_some_and(|t| Instant::now() >= t) {
            self.restart_at = None;
            self.start_buffer();
            self.update_tray();
        }
        // A recorder that has run fine for a minute resets the backoff.
        if let Some(rec) = &self.recorder {
            if rec.session.start_instant().is_some_and(|s| s.elapsed() > Duration::from_secs(60)) {
                self.restart_attempts = 0;
            }
        }

        // Settings edited by hand while running? Pick them up.
        if self.last_status.elapsed() >= Duration::from_secs(1) {
            let mtime = Settings::file_mtime();
            if mtime != self.settings_mtime {
                self.settings_mtime = mtime;
                log::info!("settings.json changed on disk; reloading");
                self.reload_settings();
            }
            self.send_status();
            self.last_status = Instant::now();
        }
    }

    fn shutdown(&mut self) {
        log::info!("quitting");
        self.stop_buffer();
        if let Some(mut ui) = self.ui.take() {
            let _ = ui.child.kill();
        }
        post_to_main(MainRequest::Quit);
    }

    // ---- replay buffer -------------------------------------------------

    fn target_monitor(&self) -> isize {
        match self.settings.capture.mode {
            CaptureMode::FollowFocus if self.focused_monitor != 0 => self.focused_monitor,
            CaptureMode::FollowFocus => foreground_monitor(),
            CaptureMode::Fixed => crate::capture::monitor_by_name(&self.settings.capture.fixed_monitor)
                .map(|m| m.handle)
                .unwrap_or(0),
        }
    }

    fn start_buffer(&mut self) {
        if self.recorder.is_some() {
            return;
        }
        let v = &self.settings.video;
        let (w, h) = crate::capture::output_size(v.output_height);
        let monitor = self.target_monitor();
        self.session_counter += 1;
        let cfg = RecorderConfig {
            encoder: v.encoder,
            width: w,
            height: h,
            fps: v.fps,
            bitrate_kbps: v.bitrate_kbps,
            buffer_seconds: self.settings.buffer_seconds,
            segment_seconds: 1,
            session_dir: crate::paths::buffer_root().join(format!("session_{}", self.session_counter)),
            gpu_upload: self.settings.hardware.as_ref().is_some_and(|h| h.gpu_upload.contains(&v.encoder)),
            audio_bitrate_kbps: self.settings.audio.bitrate_kbps,
        };
        let audio: Option<crate::audio::AudioSourceFactory> = self.settings.audio.record_pc_audio.then(|| {
            Box::new(|| Box::new(crate::audio::loopback::LoopbackSource::new()) as Box<dyn crate::audio::AudioSource>)
                as crate::audio::AudioSourceFactory
        });
        let cursor = self.settings.capture.capture_cursor;
        let source: SourceFactory =
            Box::new(move || Ok(Box::new(WgcSource::new(monitor, w, h, cursor)?) as Box<dyn FrameSource>));
        log::info!(
            "starting replay buffer: {}s, {}x{} @ {} fps, {} kbps, {}, monitor {}",
            self.settings.buffer_seconds,
            w,
            h,
            v.fps,
            v.bitrate_kbps,
            v.encoder.label(),
            monitor_name(monitor)
        );
        match Recorder::start(cfg, source, audio) {
            Ok(r) => {
                self.recorder = Some(r);
                self.note = "Replay buffer running".into();
            }
            Err(e) => {
                self.note = format!("Could not start recording: {e:#}");
                log::error!("{}", self.note);
            }
        }
    }

    fn stop_buffer(&mut self) {
        if let Some(r) = self.recorder.take() {
            r.stop();
        }
    }

    fn save_clip(&mut self) {
        let pressed_at = Instant::now();
        let Some(rec) = &self.recorder else {
            self.note = "Replay buffer is off: nothing to save".into();
            log::warn!("{}", self.note);
            error_beep();
            return;
        };
        let _ = self.saver.send(SaveJob {
            session: rec.session.clone(),
            pressed_at,
            seconds: self.settings.buffer_seconds as f64,
            out_dir: self.settings.clips_dir.clone(),
        });
    }

    fn screenshot(&mut self) {
        let dir = self.settings.screenshots_dir.clone();
        let events = self.events.clone();
        let handle = self.recorder.as_ref().filter(|r| r.session.start_instant().is_some()).map(|r| r.screenshot_handle());
        let monitor = self.target_monitor();
        let cursor = self.settings.capture.capture_cursor;
        // Grab the pixels quickly, encode the PNG on a worker thread.
        std::thread::spawn(move || {
            let result = (|| -> Result<PathBuf> {
                let img = match handle {
                    Some(h) => h.take()?,
                    None => crate::capture::screenshot::capture_once(monitor, cursor)?,
                };
                crate::capture::screenshot::save_screenshot(&img, &dir)
            })();
            let _ = events.send(Event::ScreenshotDone(result.map_err(|e| format!("{e:#}"))));
        });
    }

    // ---- focus tracking ----------------------------------------------------

    fn on_foreground(&mut self, monitor: isize, pid: u32, class: &str) {
        // Ignore our own UI and shell transients (Alt+Tab switcher, Start
        // menu, notification popups...) that briefly take focus.
        const TRANSIENT: &[&str] = &[
            "ForegroundStaging",
            "MultitaskingViewFrame",
            "XamlExplorerHostIslandWindow",
            "TaskSwitcherWnd",
            "Windows.UI.Core.CoreWindow",
            "NotifyIconOverflowWindow",
            "TopLevelWindowForOverflowXamlIsland",
        ];
        let ui_pid = self.ui.as_ref().map(|u| u.child.id());
        if monitor == 0 || Some(pid) == ui_pid || TRANSIENT.contains(&class) {
            return;
        }
        self.focused_monitor = monitor;
        if self.settings.capture.mode == CaptureMode::FollowFocus {
            self.pending_switch = Some((monitor, Instant::now()));
        }
    }

    // ---- settings ------------------------------------------------------------

    fn save_settings(&mut self) {
        if let Err(e) = self.settings.save() {
            log::error!("saving settings: {e:#}");
        }
        self.settings_mtime = Settings::file_mtime();
    }

    fn reload_settings(&mut self) {
        let (new, _) = Settings::load();
        self.settings_mtime = Settings::file_mtime();
        let old = std::mem::replace(&mut self.settings, new);
        if old.hotkeys != self.settings.hotkeys {
            self.register_hotkeys();
        }
        let needs_restart = old.video != self.settings.video
            || old.audio != self.settings.audio
            || old.buffer_seconds != self.settings.buffer_seconds
            || old.capture.capture_cursor != self.settings.capture.capture_cursor;
        if needs_restart && self.recorder.is_some() {
            log::info!("video settings changed; restarting replay buffer");
            self.stop_buffer();
            self.start_buffer();
        } else if old.capture != self.settings.capture {
            let target = self.target_monitor();
            if let Some(rec) = &self.recorder {
                rec.switch_monitor(target);
            }
        }
        self.update_tray();
    }

    fn register_hotkeys(&mut self) {
        post_to_main(MainRequest::RegisterHotkeys {
            clip: self.settings.hotkeys.clip.clone(),
            screenshot: self.settings.hotkeys.screenshot.clone(),
        });
    }

    fn update_tray(&self) {
        let recording = self.recorder.is_some();
        let state = if recording { "recording" } else { "stopped" };
        let tooltip = if self.note.is_empty() {
            format!("ClipApp: {state}")
        } else {
            format!("ClipApp: {state}\n{}", self.note)
        };
        post_to_main(MainRequest::UpdateTray {
            recording,
            tooltip,
            clip_key: self.settings.hotkeys.clip.clone(),
            shot_key: self.settings.hotkeys.screenshot.clone(),
        });
    }

    // ---- UI process ----------------------------------------------------

    fn status(&self) -> CoreStatus {
        let mut s = CoreStatus {
            recording: self.recorder.is_some(),
            note: self.note.clone(),
            last_clip: self.last_clip.clone(),
            hotkey_notes: self.hotkey_notes.clone(),
            ..Default::default()
        };
        match &self.recorder {
            Some(r) => {
                let ss = &r.session;
                s.state = if ss.start_instant().is_some() { "Recording".into() } else { "Starting...".into() };
                s.encoder = ss.encoder.label().into();
                s.resolution = format!("{}x{}", ss.width, ss.height);
                s.fps = ss.fps;
                s.monitor = monitor_name(ss.monitor.load(Ordering::Relaxed));
                s.frames_encoded = ss.stats.frames_encoded.load(Ordering::Relaxed);
                s.frames_dropped = ss.stats.frames_dropped.load(Ordering::Relaxed);
                s.audio = ss.audio;
                s.buffered_seconds = ss.buffered_seconds(self.settings.buffer_seconds);
            }
            None => s.state = "Stopped".into(),
        }
        s
    }

    fn send_status(&mut self) {
        let line = super::ipc::encode(&CoreToUi::Status(self.status()));
        self.send_to_ui(&line);
    }

    fn send_to_ui(&mut self, line: &str) {
        let alive = match self.ui.as_mut() {
            Some(ui) => ui.child.try_wait().ok().flatten().is_none() && ui.stdin.write_all(line.as_bytes()).is_ok(),
            None => return,
        };
        if !alive {
            self.ui = None; // the window was closed
        }
    }

    /// Shows the UI on `tab`, starting the UI process if needed.
    /// `activate`: bring it to the front. `minimized`: start minimized
    /// (used after a clip is saved mid-game, so the game keeps focus).
    fn open_ui(&mut self, tab: &str, clip: Option<&Path>, activate: bool, minimized: bool) {
        let msg = CoreToUi::Show {
            tab: tab.into(),
            clip: clip.map(Path::to_path_buf),
            activate,
        };
        if let Some(ui) = &self.ui {
            if activate {
                allow_foreground(ui.child.id());
            }
            self.send_to_ui(&super::ipc::encode(&msg));
            if self.ui.is_some() {
                return;
            }
        }
        match self.spawn_ui(tab, clip, minimized) {
            Ok(ui) => {
                allow_foreground(ui.child.id());
                self.ui = Some(ui);
                self.send_status();
            }
            Err(e) => log::error!("could not open the window: {e:#}"),
        }
    }

    fn spawn_ui(&self, tab: &str, clip: Option<&Path>, minimized: bool) -> Result<UiProcess> {
        let exe = std::env::current_exe()?;
        let mut cmd = crate::win::hidden_command(exe);
        cmd.args(["--ui", "--tab", tab]);
        if let Some(c) = clip {
            cmd.arg("--clip").arg(c);
        }
        if minimized {
            cmd.arg("--minimized");
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("starting UI process")?;
        let stdin = child.stdin.take().context("no UI stdin")?;
        let stdout = child.stdout.take().context("no UI stdout")?;
        let events = self.events.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(|l| l.ok()) {
                match serde_json::from_str::<UiToCore>(&line) {
                    Ok(cmd) => {
                        if events.send(Event::FromUi(cmd)).is_err() {
                            break;
                        }
                    }
                    Err(e) => log::debug!("ignoring UI output {line:?}: {e}"),
                }
            }
        });
        Ok(UiProcess { child, stdin })
    }
}

fn foreground_monitor() -> isize {
    // SAFETY: GetForegroundWindow has no preconditions.
    let hwnd = unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
    if hwnd.0.is_null() {
        return crate::capture::primary_monitor().map(|m| m.handle).unwrap_or(0);
    }
    crate::capture::monitor_of_window(hwnd)
}

fn monitor_name(handle: isize) -> String {
    crate::capture::monitor_info(windows::Win32::Graphics::Gdi::HMONITOR(handle as *mut _))
        .map(|m| m.label())
        .unwrap_or_else(|| format!("{handle:#x}"))
}

/// Lets the UI process take the foreground (Windows only allows that for
/// processes the user just interacted with, which is us, not the child).
fn allow_foreground(pid: u32) {
    // SAFETY: plain Win32 call.
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(pid);
    }
}

fn error_beep() {
    // SAFETY: plain Win32 call.
    unsafe {
        let _ = windows::Win32::System::Diagnostics::Debug::MessageBeep(
            windows::Win32::UI::WindowsAndMessaging::MB_ICONHAND,
        );
    }
}
