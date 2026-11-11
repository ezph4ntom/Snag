//! The replay buffer: a constant-frame-rate pacer feeding the encoder, which
//! writes a rolling window of 1-second segments to disk.
//!
//! ```text
//!   WGC (screen changes, any rate)
//!     |  newest frame only
//!     v
//!   pacer thread (exactly `fps` ticks/s, locked to the wall clock)
//!     |  one NV12 frame per tick, never more, never fewer
//!     v  bounded queue (3 frames)
//!   writer thread -> ffmpeg stdin -> HLS muxer -> seg_000123.m4s files
//! ```
//!
//! **Why a pacer?** Capture delivers frames irregularly (only when the screen
//! changes). Encoders and players want constant frame rate. The pacer ticks at
//! `fps`, and each tick emits the newest frame, or repeats the previous one if
//! nothing changed. Tick `n` is scheduled at `start + n/fps`, so if the thread
//! is ever late it knows exactly how many ticks it missed. Those ticks are
//! filled with the last frame (to keep timestamps correct) and **logged as
//! dropped**, never hidden.
//!
//! **Audio** (optional) runs on its own thread: captured packets are placed
//! on the same clock as the video ticks by their timestamps, and silent
//! stretches (when nothing plays, Windows delivers no packets at all) are
//! filled in, so the audio track is continuous and stays in sync.

pub mod clip;
pub mod playlist;
pub mod source;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::audio::{AudioPacket, AudioSourceFactory, frames_from_100ns, qpc_now_100ns};
use crate::capture::screenshot::BgraImage;
use crate::encode::{AudioSink, EncoderConfig, EncoderKind, Nv12Frame};
use source::{FrameSource, Produced};

/// Settings for one recording session.
#[derive(Clone, Debug)]
pub struct RecorderConfig {
    pub encoder: EncoderKind,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub buffer_seconds: u32,
    pub segment_seconds: u32,
    /// Folder for this session's segments; created, and deleted on stop.
    pub session_dir: PathBuf,
    /// See `EncoderConfig::gpu_upload`.
    pub gpu_upload: bool,
    /// AAC bitrate when recording audio.
    pub audio_bitrate_kbps: u32,
}

/// Counters the UI and logs show. All monotonic since session start.
#[derive(Default)]
pub struct Stats {
    /// Frames handed to the encoder.
    pub frames_encoded: AtomicU64,
    /// Ticks missed because the pacer ran late (filled with a repeat).
    pub frames_dropped: AtomicU64,
    /// Ticks where the screen hadn't changed (legit repeats, not drops).
    pub frames_unchanged: AtomicU64,
}

/// Shared, read-only-ish view of a running session. The clip saver and the
/// UI status use this; only the pacer/writer threads write to it.
pub struct Session {
    pub dir: PathBuf,
    pub fps: u32,
    pub segment_seconds: u32,
    pub width: u32,
    pub height: u32,
    pub encoder: EncoderKind,
    pub stats: Stats,
    /// HMONITOR currently captured (0 for synthetic sources).
    pub monitor: AtomicIsize,
    /// Whether this session records an audio track.
    pub audio: bool,
    start: OnceLock<Instant>,
    /// The same instant as `start`, on the performance-counter clock in
    /// 100 ns units (what audio packet timestamps use).
    start_qpc: OnceLock<u64>,
    running: AtomicBool,
    error: Mutex<Option<String>>,
}

impl Session {
    /// Wall-clock instant of frame 0, once recording has actually begun.
    pub fn start_instant(&self) -> Option<Instant> {
        self.start.get().copied()
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Why the session stopped on its own, if it did.
    pub fn error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|e| e.clone())
    }

    /// Seconds of footage currently available to clip.
    pub fn buffered_seconds(&self, buffer_seconds: u32) -> f64 {
        self.start_instant()
            .map(|s| s.elapsed().as_secs_f64().min(buffer_seconds as f64))
            .unwrap_or(0.0)
    }

    fn fail(&self, msg: String) {
        log::error!("recorder failed: {msg}");
        if let Ok(mut e) = self.error.lock() {
            e.get_or_insert(msg);
        }
        self.running.store(false, Ordering::Relaxed);
    }
}

enum Command {
    SwitchMonitor(isize),
    Screenshot(Sender<Result<BgraImage>>),
    Stop,
}

/// Creates the frame source on the pacer thread (WGC objects are happiest
/// being created on the thread that uses them).
pub type SourceFactory = Box<dyn FnOnce() -> Result<Box<dyn FrameSource>> + Send>;

/// Handle to a running replay buffer. Dropping it stops recording.
pub struct Recorder {
    pub session: Arc<Session>,
    cmd_tx: Sender<Command>,
    thread: Option<JoinHandle<()>>,
}

impl Recorder {
    /// Starts recording in the background. Returns immediately; startup
    /// errors (no capture, encoder failed) show up in `session.error()`.
    /// With `audio`, the clips also get an audio track from that source.
    pub fn start(cfg: RecorderConfig, source: SourceFactory, audio: Option<AudioSourceFactory>) -> Result<Recorder> {
        if !cfg.width.is_multiple_of(2) || !cfg.height.is_multiple_of(2) || cfg.fps == 0 {
            bail!("invalid recorder size/fps {}x{}@{}", cfg.width, cfg.height, cfg.fps);
        }
        let _ = std::fs::remove_dir_all(&cfg.session_dir);
        std::fs::create_dir_all(&cfg.session_dir)
            .with_context(|| format!("creating {}", cfg.session_dir.display()))?;
        let session = Arc::new(Session {
            dir: cfg.session_dir.clone(),
            fps: cfg.fps,
            segment_seconds: cfg.segment_seconds,
            width: cfg.width,
            height: cfg.height,
            encoder: cfg.encoder,
            stats: Stats::default(),
            monitor: AtomicIsize::new(0),
            audio: audio.is_some(),
            start: OnceLock::new(),
            start_qpc: OnceLock::new(),
            running: AtomicBool::new(true),
            error: Mutex::new(None),
        });
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let s = session.clone();
        let thread = std::thread::Builder::new().name("pacer".into()).spawn(move || {
            if let Err(e) = pacer_main(&cfg, &s, source, audio, cmd_rx) {
                s.fail(format!("{e:#}"));
            }
            s.running.store(false, Ordering::Relaxed);
        })?;
        Ok(Recorder {
            session,
            cmd_tx,
            thread: Some(thread),
        })
    }

    pub fn switch_monitor(&self, monitor: isize) {
        let _ = self.cmd_tx.send(Command::SwitchMonitor(monitor));
    }

    /// Native-resolution copy of the latest captured frame.
    pub fn screenshot(&self) -> Result<BgraImage> {
        self.screenshot_handle().take()
    }

    /// A handle other threads can use to request screenshots.
    pub fn screenshot_handle(&self) -> ScreenshotHandle {
        ScreenshotHandle(self.cmd_tx.clone())
    }

    /// Stops recording, finalizes the encoder and deletes the buffer files.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        let _ = self.cmd_tx.send(Command::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_dir_all(&self.session.dir);
    }
}

/// Cloneable, `Send` way to ask a running recorder for a screenshot.
#[derive(Clone)]
pub struct ScreenshotHandle(Sender<Command>);

impl ScreenshotHandle {
    pub fn take(&self) -> Result<BgraImage> {
        let (tx, rx) = mpsc::channel();
        self.0
            .send(Command::Screenshot(tx))
            .ok()
            .context("recorder is not running")?;
        rx.recv_timeout(Duration::from_secs(3)).context("screenshot timed out")?
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Reusable NV12 buffers. A buffer is free again once only the pool holds it
/// (`Arc::strong_count == 1`), i.e. the writer is done with it.
struct FramePool {
    free: Vec<Arc<Nv12Frame>>,
    w: u32,
    h: u32,
    allocated: usize,
}

impl FramePool {
    fn take(&mut self) -> Arc<Nv12Frame> {
        if let Some(i) = self.free.iter().position(|a| Arc::strong_count(a) == 1) {
            return self.free.swap_remove(i);
        }
        self.allocated += 1;
        if self.allocated == 12 {
            log::warn!("frame pool grew to 12 buffers: the encoder is falling behind");
        }
        Arc::new(Nv12Frame::new(self.w, self.h))
    }
}

fn set_thread_priority(highest: bool) {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_HIGHEST,
    };
    let p = if highest { THREAD_PRIORITY_HIGHEST } else { THREAD_PRIORITY_ABOVE_NORMAL };
    // SAFETY: pseudo-handle of the current thread.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), p);
    }
}

fn pacer_main(
    cfg: &RecorderConfig,
    session: &Arc<Session>,
    make_source: SourceFactory,
    make_audio: Option<AudioSourceFactory>,
    cmd_rx: Receiver<Command>,
) -> Result<()> {
    // WinRT (capture API) wants the thread in the multithreaded apartment.
    // SAFETY: plain initialization call; "already initialized" is fine.
    unsafe {
        let _ = windows::Win32::System::WinRT::RoInitialize(windows::Win32::System::WinRT::RO_INIT_MULTITHREADED);
    }
    // The pacer does very little work but must do it on time.
    set_thread_priority(true);

    let mut source = make_source()?;
    let (w, h) = source.size();
    if (w, h) != (cfg.width, cfg.height) {
        bail!("source size {w}x{h} doesn't match encoder size {}x{}", cfg.width, cfg.height);
    }
    session.monitor.store(source.current_monitor(), Ordering::Relaxed);
    let dt = Duration::from_secs_f64(1.0 / cfg.fps as f64);
    let mut pool = FramePool { free: Vec::new(), w, h, allocated: 0 };

    // Warm-up: don't start the encoder until there's a real frame, so a clip
    // can never begin with a black or uninitialized frame.
    let warm_start = Instant::now();
    let mut last: Arc<Nv12Frame> = loop {
        let mut buf = pool.take();
        let frame = Arc::get_mut(&mut buf).expect("pool buffer is unique");
        if source.produce(0, frame)? == Produced::New {
            pool.free.push(buf.clone());
            break buf;
        }
        pool.free.push(buf);
        if warm_start.elapsed() > Duration::from_secs(5) {
            bail!("no frames from capture within 5 s (is the screen locked or showing protected content?)");
        }
        std::thread::sleep(Duration::from_millis(5));
    };

    let enc_cfg = EncoderConfig {
        kind: cfg.encoder,
        width: w,
        height: h,
        fps: cfg.fps,
        bitrate_kbps: cfg.bitrate_kbps,
        segment_seconds: cfg.segment_seconds,
        segment_dir: cfg.session_dir.clone(),
        // Clip length plus slack for saves in flight.
        keep_segments: cfg.buffer_seconds.div_ceil(cfg.segment_seconds) + 4,
        gpu_upload: cfg.gpu_upload,
        audio: make_audio.is_some(),
        audio_bitrate_kbps: cfg.audio_bitrate_kbps,
    };
    let mut encoder = crate::encode::create_encoder(&enc_cfg)?;

    // Audio thread. It must be running before `prime`, because the encoder
    // reads a little audio while it starts up.
    let audio_stop = Arc::new(AtomicBool::new(false));
    let _stop_audio_on_exit = StopOnDrop(audio_stop.clone());
    let audio_thread = match (encoder.take_audio_sink(), make_audio) {
        (Some(sink), Some(make)) => {
            // Captured frames reach the encoder `latency_ticks` late; delay the
            // audio by the same amount so sound lines up with the picture.
            let delay = source.latency_ticks() as i64 * crate::audio::SAMPLE_RATE as i64 / cfg.fps as i64;
            let (s, stop) = (session.clone(), audio_stop.clone());
            Some(std::thread::Builder::new().name("audio".into()).spawn(move || audio_main(sink, make, &s, &stop, delay))?)
        }
        _ => None,
    };

    // Wait for the encoder to be ready *before* the clock starts; otherwise
    // its startup (ffmpeg loading) would make the first ticks late.
    encoder.prime(&last)?;

    // Writer thread: the only place that can block on the encoder, so a slow
    // encoder shows up as queue backpressure rather than pacer jitter.
    let (frame_tx, frame_rx): (SyncSender<Arc<Nv12Frame>>, _) = mpsc::sync_channel(3);
    let ws = session.clone();
    let writer = std::thread::Builder::new().name("encoder-writer".into()).spawn(move || {
        set_thread_priority(false);
        let mut encoder = encoder;
        let t_start = Instant::now();
        for (n, frame) in frame_rx.into_iter().enumerate() {
            let t = Instant::now();
            if let Err(e) = encoder.encode(&frame) {
                ws.fail(format!("{e:#}"));
                return;
            }
            // A write that blocks means the encoder isn't keeping up; worth
            // knowing when diagnosing drops.
            let took = t.elapsed();
            if took > Duration::from_millis(20) {
                log::warn!(
                    "encoder blocked {} ms on frame {n} ({} ms after start)",
                    took.as_millis(),
                    t.duration_since(t_start).as_millis()
                );
            }
            ws.stats.frames_encoded.fetch_add(1, Ordering::Relaxed);
        }
        if let Err(e) = encoder.finish() {
            log::warn!("encoder finish: {e:#}");
        }
    })?;

    let start = Instant::now();
    let _ = session.start_qpc.set(qpc_now_100ns());
    let _ = session.start.set(start);
    log::info!("recording at {w}x{h} @ {} fps ({} after first frame wait)", cfg.fps, fmt_ms(warm_start.elapsed()));

    let mut tick: u64 = 0;
    let mut drops_this_window: u64 = 0;
    let mut window_start = Instant::now();
    let result: Result<()> = 'run: loop {
        // Commands are handled between ticks (they're rare and quick).
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                Command::Stop => break 'run Ok(()),
                Command::SwitchMonitor(m) => match source.switch_monitor(m) {
                    Ok(()) => session.monitor.store(source.current_monitor(), Ordering::Relaxed),
                    Err(e) => log::warn!("monitor switch failed: {e:#}"),
                },
                Command::Screenshot(reply) => {
                    let _ = reply.send(source.screenshot());
                }
            }
        }

        // Sleep until this tick's deadline. (Rust's sleep on Windows uses a
        // high-resolution waitable timer, accurate to well under 1 ms.)
        let deadline = start + dt.mul_f64(tick as f64);
        let now = Instant::now();
        if deadline > now {
            std::thread::sleep(deadline - now);
        }

        // Late by one or more whole frames? Fill those ticks with the last
        // frame so timestamps stay true, and record them as dropped.
        let due = (start.elapsed().as_secs_f64() * cfg.fps as f64).floor() as u64;
        if due > tick {
            let missed = due - tick;
            for _ in 0..missed {
                if frame_tx.send(last.clone()).is_err() {
                    break 'run Ok(()); // writer died; its error is already recorded
                }
            }
            session.stats.frames_dropped.fetch_add(missed, Ordering::Relaxed);
            drops_this_window += missed;
            tick = due;
        }

        let mut buf = pool.take();
        let produced = match source.produce(tick, Arc::get_mut(&mut buf).expect("pool buffer is unique")) {
            Ok(p) => p,
            Err(e) => break 'run Err(e),
        };
        match produced {
            Produced::New => {
                pool.free.push(buf.clone());
                last = buf;
            }
            Produced::Unchanged => {
                pool.free.push(buf);
                session.stats.frames_unchanged.fetch_add(1, Ordering::Relaxed);
            }
        }
        if frame_tx.send(last.clone()).is_err() {
            break 'run Ok(());
        }
        tick += 1;

        // Drops are aggregated per second so a stall logs one line, not 60.
        if window_start.elapsed() >= Duration::from_secs(1) {
            if drops_this_window > 0 {
                let total = session.stats.frames_dropped.load(Ordering::Relaxed);
                log::warn!(
                    "DROPPED {drops_this_window} frame(s) in the last second (pacer ran late; filled with repeats to keep timing). Total dropped: {total}"
                );
            }
            drops_this_window = 0;
            window_start = Instant::now();
        }
    };

    // Close the audio pipe and the frame queue; ffmpeg finalizes once both
    // inputs have ended.
    audio_stop.store(true, Ordering::Relaxed);
    drop(frame_tx);
    let _ = writer.join();
    if let Some(t) = audio_thread {
        let _ = t.join();
    }
    let st = &session.stats;
    log::info!(
        "recording stopped: {} frames encoded, {} dropped, {} unchanged-screen repeats",
        st.frames_encoded.load(Ordering::Relaxed),
        st.frames_dropped.load(Ordering::Relaxed),
        st.frames_unchanged.load(Ordering::Relaxed)
    );
    result
}

/// Sets the flag when dropped, so the audio thread also stops when the pacer
/// exits early with an error.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Places audio on the video timeline and feeds it to the encoder.
///
/// Timeline frame `n` (audio frame = one stereo sample pair) is at
/// `start + n / 48000 s`, exactly like video frame `k` is at `start + k/fps`.
/// Each captured packet is written at the position its timestamp says;
/// gaps (nothing playing) become silence. Silence is only filled up to
/// 50 ms behind real time, so packets that arrive a little late still fit.
fn audio_main(
    mut sink: Box<dyn AudioSink>,
    make_source: AudioSourceFactory,
    session: &Arc<Session>,
    stop: &AtomicBool,
    delay_frames: i64,
) {
    const TOLERANCE: i64 = 480; // 10 ms of jitter is treated as contiguous
    // Fill silence up to 50 ms behind real time. Audio must never trail the
    // video by ~100 ms or more: ffmpeg would then pause reading video.
    const MARGIN: i64 = 2400;
    // SAFETY: COM must be initialized on every thread that uses WASAPI.
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
    }
    set_thread_priority(false);
    let preroll = match sink.open() {
        Ok(frames) => frames as i64,
        Err(e) => {
            if !stop.load(Ordering::Relaxed) {
                session.fail(format!("audio: {e:#}"));
            }
            return;
        }
    };
    let mut source = make_source();
    let mut packets: Vec<AudioPacket> = Vec::new();

    // Wait for the video clock to start; drop whatever plays before that.
    let start = loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        if let Some(&s) = session.start_qpc.get() {
            break s;
        }
        let now = qpc_now_100ns();
        source.poll(&mut packets, now, now);
        packets.clear();
        std::thread::sleep(Duration::from_millis(2));
    };

    let mut written: i64 = preroll;
    let silence = vec![0.0f32; 4800 * crate::audio::CHANNELS];
    let write_silence = |sink: &mut Box<dyn AudioSink>, mut frames: i64| -> Result<()> {
        while frames > 0 {
            let n = frames.min(4800) as usize;
            sink.write(&silence[..n * crate::audio::CHANNELS])?;
            frames -= n as i64;
        }
        Ok(())
    };
    let result: Result<()> = (|| {
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(10));
            let now = qpc_now_100ns();
            source.poll(&mut packets, start, now);
            for p in packets.drain(..) {
                let frames = (p.samples.len() / crate::audio::CHANNELS) as i64;
                let pos = frames_from_100ns(p.qpc_100ns as i128 - start as i128) + delay_frames;
                let mut skip = 0;
                if pos > written + TOLERANCE {
                    write_silence(&mut sink, pos - written)?; // quiet gap before this packet
                    written = pos;
                } else if pos < written - TOLERANCE {
                    skip = (written - pos).min(frames); // already covered (overlap)
                }
                if skip < frames {
                    sink.write(&p.samples[skip as usize * crate::audio::CHANNELS..])?;
                    written += frames - skip;
                }
            }
            let target = frames_from_100ns(now as i128 - start as i128) + delay_frames - MARGIN;
            if target > written {
                write_silence(&mut sink, target - written)?;
                written = target;
            }
        }
        Ok(())
    })();
    if let Err(e) = result {
        if !stop.load(Ordering::Relaxed) {
            session.fail(format!("audio: {e:#}"));
        }
    }
}

fn fmt_ms(d: Duration) -> String {
    format!("{} ms", d.as_millis())
}
