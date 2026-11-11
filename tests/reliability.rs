//! Reliability tests: save 50 clips in a row while recording continues, and
//! check every frame of every clip.
//!
//! These are `#[ignore]`d because they take a minute or two each and need
//! ffmpeg. Run them with:
//!
//! ```text
//! cargo test --release --test reliability -- --ignored --nocapture
//! ```
//!
//! Choose the encoder with `CLIPAPP_TEST_ENCODER=nvenc|amf|qsv|x264`
//! (default: the best one that works on this machine).

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use clipapp::audio::{AudioSource, AudioSourceFactory, BeepSource};
use clipapp::buffer::clip::save_clip;
use clipapp::buffer::source::{FrameSource, SyntheticSource, WgcSource};
use clipapp::buffer::{Recorder, RecorderConfig, SourceFactory};
use clipapp::encode::EncoderKind;
use clipapp::verify::{ClipReport, Crop, verify_clip};

const CLIPS: usize = 50;
const CLIP_SECONDS: f64 = 4.0;
/// Time between clip requests. Shorter than a clip, so consecutive clips
/// overlap and saves happen back to back while the buffer keeps recording.
const INTERVAL: Duration = Duration::from_millis(1000);
const FPS: u32 = 60;

/// Returns the encoder to test and whether its D3D11 upload path works.
fn pick_encoder() -> (EncoderKind, bool) {
    let wanted = match std::env::var("CLIPAPP_TEST_ENCODER").as_deref() {
        Ok("nvenc") => Some(EncoderKind::Nvenc),
        Ok("amf") => Some(EncoderKind::Amf),
        Ok("qsv") => Some(EncoderKind::Qsv),
        Ok("x264") => Some(EncoderKind::X264),
        _ => None,
    };
    let probes = clipapp::encode::detect::probe_all();
    let p = probes
        .iter()
        .find(|p| p.works && wanted.is_none_or(|w| w == p.kind))
        .unwrap_or_else(|| panic!("encoder {wanted:?} is not available on this machine"));
    (p.kind, p.gpu_upload)
}

fn scratch_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("clipapp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Records with `source`, saves `CLIPS` clips one per `INTERVAL`, verifies
/// each one, and returns all reports.
#[allow(clippy::too_many_arguments)] // a test helper; named args read fine here
fn run(
    name: &str,
    width: u32,
    height: u32,
    source: SourceFactory,
    audio: AudioSourceFactory,
    consecutive: bool,
    crop: Option<Crop>,
    verify_during_recording: bool,
) -> Vec<ClipReport> {
    clipapp::logging::init(&format!("test-{name}"));
    clipapp::encode::detect::ffmpeg_version().expect("ffmpeg must be installed (see README)");
    let (encoder, gpu_upload) = pick_encoder();
    let root = scratch_dir(name);
    let clips_dir = root.join("clips");
    println!(
        "[{name}] encoder: {}{}, {width}x{height}@{FPS}, output in {}",
        encoder.label(),
        if gpu_upload { " (D3D11 upload)" } else { "" },
        root.display()
    );

    let rec = Recorder::start(
        RecorderConfig {
            encoder,
            width,
            height,
            fps: FPS,
            bitrate_kbps: 12_000,
            buffer_seconds: CLIP_SECONDS as u32,
            segment_seconds: 1,
            session_dir: root.join("session"),
            gpu_upload,
            audio_bitrate_kbps: 160,
        },
        source,
        Some(audio),
    )
    .expect("recorder start");

    // Fill the buffer before the first clip.
    let warm = Instant::now();
    while rec.session.buffered_seconds(CLIP_SECONDS as u32) < CLIP_SECONDS {
        assert!(rec.session.error().is_none(), "recorder failed: {:?}", rec.session.error());
        assert!(warm.elapsed() < Duration::from_secs(20), "recorder never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(500));

    // Verify on two worker threads, either while we keep saving (extra CPU
    // load as a stress test) or after recording stops.
    let verify_after = !verify_during_recording;
    let mut deferred = Vec::new();
    let (job_tx, job_rx) = mpsc::channel::<PathBuf>();
    let job_rx = std::sync::Arc::new(std::sync::Mutex::new(job_rx));
    let (rep_tx, rep_rx) = mpsc::channel::<ClipReport>();
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let (rx, tx) = (job_rx.clone(), rep_tx.clone());
            std::thread::spawn(move || {
                loop {
                    let Ok(path) = rx.lock().unwrap().recv() else { break };
                    let report = verify_clip(&path, consecutive, crop).expect("verify");
                    tx.send(report).unwrap();
                }
            })
        })
        .collect();
    drop(rep_tx);

    let started = Instant::now();
    let mut save_times = Vec::new();
    for i in 0..CLIPS {
        let pressed = started + INTERVAL * i as u32;
        if let Some(wait) = pressed.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        let r = save_clip(&rec.session, pressed, CLIP_SECONDS, &clips_dir).unwrap_or_else(|e| panic!("clip {i}: {e:#}"));
        assert!(r.warnings.is_empty(), "clip {i} warnings: {:?}", r.warnings);
        save_times.push(r.took);
        if verify_after {
            deferred.push(r.path);
        } else {
            job_tx.send(r.path).unwrap();
        }
    }

    let stats = &rec.session.stats;
    let dropped = stats.frames_dropped.load(std::sync::atomic::Ordering::Relaxed);
    let encoded = stats.frames_encoded.load(std::sync::atomic::Ordering::Relaxed);
    let unchanged = stats.frames_unchanged.load(std::sync::atomic::Ordering::Relaxed);
    assert!(rec.session.error().is_none(), "recorder error: {:?}", rec.session.error());
    rec.stop();
    for p in deferred {
        job_tx.send(p).unwrap();
    }
    drop(job_tx);
    for w in workers {
        w.join().unwrap();
    }

    let mut reports: Vec<ClipReport> = rep_rx.into_iter().collect();
    reports.sort_by(|a, b| a.path.cmp(&b.path));
    let max_save = save_times.iter().max().unwrap();
    let avg_save = save_times.iter().sum::<Duration>() / save_times.len() as u32;
    println!(
        "[{name}] pipeline: {encoded} frames encoded, {dropped} dropped, {unchanged} unchanged-screen repeats; save time avg {} ms, max {} ms",
        avg_save.as_millis(),
        max_save.as_millis()
    );
    for r in &reports {
        println!("[{name}] {} {}", if r.is_ok() { "OK  " } else { "FAIL" }, r.summary());
    }
    assert_eq!(reports.len(), CLIPS);
    assert_eq!(dropped, 0, "the pacer dropped frames (see log)");
    reports
}

fn assert_all_ok(reports: &[ClipReport]) {
    let bad: Vec<_> = reports.iter().filter(|r| !r.is_ok()).map(|r| r.summary()).collect();
    assert!(bad.is_empty(), "{} of {} clips failed:\n{}", bad.len(), reports.len(), bad.join("\n"));
    for r in reports {
        // At least the requested length (segments are whole seconds, so up to +1 s).
        assert!(r.frames as f64 >= CLIP_SECONDS * FPS as f64, "{} too short", r.summary());
        assert!((r.fps - FPS as f64).abs() < 0.01, "{} wrong fps", r.summary());
        // The audio track covers the whole clip (AAC works in 21 ms blocks).
        let audio = r.audio_seconds.unwrap_or_else(|| panic!("{} has no audio", r.summary()));
        let video = r.frames as f64 / FPS as f64;
        assert!((audio - video).abs() < 0.1, "{}: audio {audio:.3} s vs video {video:.3} s", r.summary());
    }
    // All good: delete the clips (the live ones contain a recording of the
    // whole screen). On failure they're kept for inspection.
    if let Some(root) = reports[0].path.parent().and_then(|clips| clips.parent()) {
        let _ = std::fs::remove_dir_all(root);
    }
}

/// Headless: synthetic frames numbered by tick. Every clip must contain
/// exactly consecutive frame numbers: no drops, duplicates or black frames
/// anywhere in pacer -> encoder -> segments -> clip.
#[test]
#[ignore = "takes ~1 min and needs ffmpeg; run with --ignored"]
fn fifty_clips_synthetic() {
    let (w, h) = (1920, 1080);
    let source: SourceFactory = Box::new(move || Ok(Box::new(SyntheticSource::new(w, h)) as Box<dyn FrameSource>));
    // Clips are decoded on all cores *during* recording: a stress test for
    // the pacer and encoder (any late tick would show up as a gap).
    let audio: AudioSourceFactory = Box::new(|| Box::new(BeepSource::default()) as Box<dyn AudioSource>);
    let reports = run("synthetic", w, h, source, audio, true, None, true);
    // A/V sync: every beep must line up with its video frame. One frame is
    // 16.7 ms; AAC encoding adds a little smearing at the edges.
    for r in &reports {
        assert!(r.av_offsets_ms.len() >= 3, "{}: beeps not found", r.summary());
        assert!(r.worst_av_offset_ms().abs() < 25.0, "{}: audio out of sync", r.summary());
    }
    assert_all_ok(&reports);
}

/// Live: real Windows Graphics Capture of the primary monitor, with a test
/// pattern window in the top-left corner that repaints every refresh. Frame
/// numbers must strictly increase (the window updates faster than we
/// sample), and no frame may be black or a repeat.
///
/// A 640x360 window flashes on screen for about a minute.
#[test]
#[ignore = "takes ~1 min, shows a test window, needs a desktop session; run with --ignored"]
fn fifty_clips_live_capture() {
    let mon = clipapp::capture::primary_monitor().expect("a monitor");
    let (w, h) = (mon.width & !1, mon.height & !1);
    let (pw, ph) = (640, 360);
    let pattern = clipapp::verify::pattern::PatternWindow::open(mon.x, mon.y, pw, ph).expect("pattern window");
    std::thread::sleep(Duration::from_millis(300));
    let (t0, p0) = (Instant::now(), pattern.paints.load(std::sync::atomic::Ordering::Relaxed));
    let handle = mon.handle;
    let source: SourceFactory =
        Box::new(move || Ok(Box::new(WgcSource::new(handle, w, h, false)?) as Box<dyn FrameSource>));
    // Verified after recording: decoding on all cores during recording makes
    // the Windows compositor itself skip frames (measured ~4.5/s), which
    // would test the OS, not our pipeline.
    // Real PC audio (WASAPI loopback); whatever is (or isn't) playing.
    let audio: AudioSourceFactory =
        Box::new(|| Box::new(clipapp::audio::loopback::LoopbackSource::new()) as Box<dyn AudioSource>);
    let reports = run("live", w, h, source, audio, false, Some((0, 0, pw as u32, ph as u32)), false);
    let paints = pattern.paints.load(std::sync::atomic::Ordering::Relaxed).wrapping_sub(p0);
    println!(
        "[live] test pattern repainted at {:.1} Hz (monitor {} Hz); {} paint gaps > 16.7 ms",
        paints as f64 / t0.elapsed().as_secs_f64(),
        mon.refresh_hz,
        pattern.long_gaps.load(std::sync::atomic::Ordering::Relaxed)
    );
    assert_all_ok(&reports);
}
