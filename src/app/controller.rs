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
use crate::config::{AfterClip, CaptureMode, RecordMode, Settings};

#[derive(Debug, Clone, Copy)]
pub enum TrayAction {
    SaveClip,
    Record,
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
    /// How the hotkeys were set up (notes, errors, hook-only keys).
    HotkeyReport(super::HotkeyReport),
    /// The key press recorded for a hotkey being rebound (None = cancelled).
    HotkeyCaptured { target: String, hotkey: Option<String> },
    ClipDone(Result<ClipResult, String>),
    /// A long recording was written to the clips folder (or failed).
    RecordingDone(Result<ClipResult, String>),
    ScreenshotDone(Result<PathBuf, String>),
    FromUi(UiToCore),
    /// A second `snag.exe` was started: show the window (on this tab).
    ShowUi(String),
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
    hotkey_report: super::HotkeyReport,
    /// The long recording in progress, if any.
    recording: Option<crate::buffer::recording::Recording>,
    /// A recording should be running (started in manual mode, or always in
    /// whole-session mode); it (re)starts as soon as the buffer is ready.
    want_recording: bool,
    /// Recordings being written to the clips folder.
    saving_recordings: u32,
    /// The Sound choices, shared live with the recorder's audio thread.
    audio_selection: std::sync::Arc<crate::audio::mixer::SharedSelection>,
    /// Shown while a program running as administrator is focused and a
    /// hotkey only works through the keyboard hook: (pid, text).
    elevated_warning: Option<(u32, String)>,
    elevated: bool,
    /// When a "press the new hotkey" capture started (it times out).
    capture_started: Option<Instant>,
    /// Taskbar theme the tray icon was drawn for.
    light_taskbar: bool,
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
            hotkey_report: Default::default(),
            recording: None,
            want_recording: false,
            saving_recordings: 0,
            audio_selection: crate::audio::mixer::SharedSelection::new(Default::default()),
            elevated_warning: None,
            elevated: crate::win::is_elevated(),
            capture_started: None,
            light_taskbar: crate::win::taskbar_is_light(),
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
            self.note = "ffmpeg is missing: install it with  winget install Gyan.FFmpeg  and restart Snag".into();
            log::error!("ffmpeg not found: {e:#}");
            // Tell the user how to fix it (on a separate thread: the dialog
            // blocks until closed).
            std::thread::spawn(|| {
                crate::win::error_box(
                    "Snag needs ffmpeg to record, and it isn't installed.\n\n\
                     To install it, open PowerShell (or Terminal) and run:\n\n\
                         winget install Gyan.FFmpeg\n\n\
                     Then quit Snag (right-click the tray icon > Quit) and start it again.\n\
                     (Or put ffmpeg.exe and ffprobe.exe in a \"bin\" folder next to snag.exe.)",
                );
            });
        }
        self.focused_monitor = foreground_monitor();
        self.register_hotkeys();
        // A recording left behind by a crash or power cut: save what it has.
        let (events, out) = (self.events.clone(), self.settings.clips_dir.clone());
        std::thread::spawn(move || {
            for path in crate::buffer::recording::recover_leftovers(&out) {
                let r = ClipResult { path, seconds: 0.0, segments: 0, bytes: 0, took: Duration::ZERO, warnings: Vec::new() };
                let _ = events.send(Event::RecordingDone(Ok(r)));
            }
        });
        self.want_recording = self.settings.recording.mode == RecordMode::WholeSession;
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
            Event::Hotkey(crate::hotkeys::ID_RECORD) | Event::Tray(TrayAction::Record) | Event::FromUi(UiToCore::Record) => {
                self.record_button()
            }
            Event::Hotkey(_) => {}
            Event::RecordingDone(result) => {
                self.saving_recordings = self.saving_recordings.saturating_sub(1);
                match result {
                    Ok(r) => {
                        log::info!("recording saved: {} ({:.0} s, {:.1} MB, {} ms)", r.path.display(), r.seconds, r.bytes as f64 / 1e6, r.took.as_millis());
                        if self.settings.sound.enabled {
                            crate::sound::play_clip_sound(self.settings.sound.file.as_deref(), self.settings.sound.volume);
                        }
                        self.note = format!("Saved {} ({})", r.path.file_name().unwrap_or_default().to_string_lossy(), fmt_duration(r.seconds));
                        self.last_clip = Some(r.path);
                    }
                    Err(e) if e.contains("discarded") => {}
                    Err(e) => {
                        log::error!("saving recording failed: {e}");
                        self.note = format!("Recording failed: {e}");
                        error_beep();
                    }
                }
                self.update_tray();
            }
            Event::Tray(TrayAction::ToggleBuffer) | Event::FromUi(UiToCore::ToggleBuffer) => {
                if self.recorder.is_some() {
                    // Stopping the buffer also ends a manual recording (saved).
                    if self.settings.recording.mode == RecordMode::Manual {
                        self.want_recording = false;
                    }
                    self.stop_buffer();
                    self.note = "Replay buffer stopped".into();
                } else {
                    self.restart_attempts = 0;
                    self.start_buffer();
                }
                self.update_tray();
            }
            Event::Tray(TrayAction::OpenClipsFolder) => crate::win::reveal_in_explorer(&self.settings.clips_dir),
            Event::Tray(TrayAction::OpenTrimmer) => {
                let clip = self.last_clip.clone();
                self.open_ui("clips", clip.as_deref(), true, false)
            }
            Event::ShowUi(tab) => {
                let clip = if tab == "clips" { self.last_clip.clone() } else { None };
                self.open_ui(&tab, clip.as_deref(), true, false)
            }
            Event::Tray(TrayAction::OpenSettings) => self.open_ui("settings", None, true, false),
            Event::Tray(TrayAction::Quit) => {}
            Event::Foreground { monitor, pid, class } => self.on_foreground(monitor, pid, &class),
            Event::HotkeyReport(report) => {
                self.hotkey_report = report;
                self.elevated_warning = None;
            }
            Event::FromUi(UiToCore::RestartAsAdmin) => {
                log::info!("restarting as administrator");
                match crate::win::restart_elevated() {
                    Ok(()) => {
                        // The new instance asks this one to quit and takes over.
                    }
                    Err(e) => {
                        log::warn!("restart as administrator: {e:#}");
                        self.note = "Restart as administrator was cancelled".into();
                    }
                }
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
                    crate::sound::play_clip_sound(self.settings.sound.file.as_deref(), self.settings.sound.volume);
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
            Event::FromUi(UiToCore::LiveAudio { audio }) => {
                if !audio.needs_restart(&self.settings.audio) {
                    self.audio_selection.set(audio);
                }
            }
            Event::FromUi(UiToCore::Preview { on }) => {
                crate::capture::preview::WANTED.store(on, Ordering::Relaxed);
            }
        }
    }

    /// Runs after every event and at least twice a second.
    fn housekeeping(&mut self) {
        self.keep_recording_in_step();
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
            // Windows switched between light and dark mode: redraw the tray icon.
            let light = crate::win::taskbar_is_light();
            if light != self.light_taskbar {
                self.light_taskbar = light;
                self.update_tray();
            }
        }
    }

    fn shutdown(&mut self) {
        log::info!("quitting");
        // A manual recording is saved before quitting; unsaved whole-session
        // footage is discarded (that's what the mode promises).
        if let Some(rec) = self.recording.take() {
            let discard = self.settings.recording.mode == RecordMode::WholeSession;
            let job = rec.stop(&self.settings.clips_dir, discard);
            match job() {
                Ok(r) => log::info!("recording saved on quit: {}", r.path.display()),
                Err(e) => log::info!("recording on quit: {e:#}"),
            }
        }
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
        self.audio_selection.set(self.settings.audio.clone());
        let selection = self.audio_selection.clone();
        let audio: Option<crate::audio::AudioSourceFactory> = self.settings.audio.record_pc_audio.then(|| {
            Box::new(move || Box::new(crate::audio::mixer::MixSource::new(selection)) as Box<dyn crate::audio::AudioSource>)
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

    // ---- long recordings ---------------------------------------------------

    /// The record button/hotkey. Manual mode: start or stop (stop saves).
    /// Whole-session mode: save everything recorded so far.
    fn record_button(&mut self) {
        match self.settings.recording.mode {
            RecordMode::Manual if self.want_recording => {
                self.want_recording = false;
                self.finish_recording(false);
                self.note = "Saving the recording...".into();
            }
            RecordMode::Manual => {
                self.want_recording = true;
                if self.recorder.is_none() {
                    self.restart_attempts = 0;
                    self.start_buffer();
                }
                self.note = "Recording".into();
                self.keep_recording_in_step();
            }
            RecordMode::WholeSession => match &self.recording {
                Some(rec) => {
                    let job = rec.snapshot_job(&self.settings.clips_dir);
                    self.run_save_job(job);
                    self.note = "Saving the session so far (recording continues)...".into();
                }
                None => {
                    self.want_recording = true;
                    if self.recorder.is_none() {
                        self.start_buffer();
                    }
                    self.note = "Session recording starts with the replay buffer".into();
                }
            },
        }
        self.update_tray();
    }

    fn run_save_job(&mut self, job: impl FnOnce() -> Result<ClipResult> + Send + 'static) {
        self.saving_recordings += 1;
        let events = self.events.clone();
        std::thread::Builder::new()
            .name("save-recording".into())
            .spawn(move || {
                let _ = events.send(Event::RecordingDone(job().map_err(|e| format!("{e:#}"))));
            })
            .ok();
    }

    /// Stops the current recording and saves it (or discards it).
    fn finish_recording(&mut self, discard: bool) {
        if let Some(rec) = self.recording.take() {
            let job = rec.stop(&self.settings.clips_dir, discard);
            self.run_save_job(job);
        }
    }

    /// Starts a recording once the buffer is running, and saves the current
    /// one when its buffer went away (stopped, restarted for new settings,
    /// or failed); a new one then starts with the new buffer.
    fn keep_recording_in_step(&mut self) {
        if self.recording.as_ref().is_some_and(|r| r.ended()) {
            log::info!("the replay buffer stopped under a recording; saving it");
            self.finish_recording(false);
        }
        if self.want_recording && self.recording.is_none() {
            let session = self.recorder.as_ref().filter(|r| r.session.start_instant().is_some()).map(|r| r.session.clone());
            if let Some(session) = session {
                match crate::buffer::recording::Recording::start(session) {
                    Ok(r) => self.recording = Some(r),
                    Err(e) => {
                        log::error!("could not start recording: {e:#}");
                        self.note = format!("Could not start recording: {e:#}");
                        self.want_recording = false;
                    }
                }
                self.update_tray();
            }
        }
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
        self.check_elevated_foreground(pid);
        self.focused_monitor = monitor;
        if self.settings.capture.mode == CaptureMode::FollowFocus {
            self.pending_switch = Some((monitor, Instant::now()));
        }
    }

    /// A program running as administrator gets focus while a hotkey only
    /// works through the keyboard hook: Windows won't deliver keys to the
    /// hook then, so say so instead of failing silently.
    fn check_elevated_foreground(&mut self, pid: u32) {
        if self.elevated || self.hotkey_report.hook_only.is_empty() {
            self.elevated_warning = None;
            return;
        }
        if self.elevated_warning.as_ref().is_some_and(|(p, _)| *p == pid) {
            return;
        }
        if crate::win::process_is_elevated(pid) {
            let name = crate::win::process_exe_name(pid).unwrap_or_else(|| format!("process {pid}"));
            let keys: Vec<String> = self.hotkey_report.hook_only.iter().map(|k| crate::hotkeys::display(k)).collect();
            let text = format!(
                "{name} runs as administrator, so {} can't reach Snag while it's focused. Restart Snag as administrator (Settings > Hotkeys) or pick a different key.",
                keys.join(" and ")
            );
            log::warn!("{text}");
            self.note = text.clone();
            self.elevated_warning = Some((pid, text));
        } else {
            self.elevated_warning = None;
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
        if old.recording.mode != self.settings.recording.mode {
            // Switching modes ends the current recording (saved).
            self.finish_recording(false);
            self.want_recording = self.settings.recording.mode == RecordMode::WholeSession;
        }
        // Which apps/devices are recorded changes live; only turning audio
        // on/off or its bitrate needs a restart.
        self.audio_selection.set(self.settings.audio.clone());
        let needs_restart = old.video != self.settings.video
            || old.audio.needs_restart(&self.settings.audio)
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
            record: self.settings.hotkeys.record.clone(),
        });
    }

    fn update_tray(&self) {
        let recording = self.recorder.is_some();
        let state = if recording { "recording" } else { "stopped" };
        let tooltip = if self.note.is_empty() {
            format!("Snag: {state}")
        } else {
            format!("Snag: {state}\n{}", self.note)
        };
        post_to_main(MainRequest::UpdateTray {
            recording,
            tooltip,
            clip_key: self.settings.hotkeys.clip.clone(),
            shot_key: self.settings.hotkeys.screenshot.clone(),
            record_label: {
                let key = &self.settings.hotkeys.record;
                match (self.settings.recording.mode, self.recording.is_some()) {
                    (RecordMode::Manual, true) => format!("Stop recording\t{key}"),
                    (RecordMode::Manual, false) => format!("Start recording\t{key}"),
                    (RecordMode::WholeSession, _) => format!("Save session so far\t{key}"),
                }
            },
        });
    }

    // ---- UI process ----------------------------------------------------

    fn status(&self) -> CoreStatus {
        let mut s = CoreStatus {
            recording: self.recorder.is_some(),
            note: self.note.clone(),
            last_clip: self.last_clip.clone(),
            hotkey_notes: self.hotkey_report.notes.clone(),
            hotkey_errors: self.hotkey_report.errors.clone(),
            hotkey_warning: self.elevated_warning.as_ref().map(|(_, t)| t.clone()),
            hook_only_hotkeys: !self.hotkey_report.hook_only.is_empty(),
            rec_active: self.recording.is_some(),
            rec_seconds: self.recording.as_ref().map(|r| r.seconds()).unwrap_or(0.0),
            rec_bytes: self.recording.as_ref().map(|r| r.bytes()).unwrap_or(0),
            rec_session_mode: self.settings.recording.mode == RecordMode::WholeSession,
            rec_saving: self.saving_recordings > 0,
            elevated: self.elevated,
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
                if ss.audio {
                    let cap = crate::audio::mixer::capture_state();
                    s.audio_sources = cap.sources;
                    s.audio_note = cap.note;
                }
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
            crate::capture::preview::WANTED.store(false, Ordering::Relaxed);
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

/// `1:02:03` / `4:05`.
fn fmt_duration(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}
