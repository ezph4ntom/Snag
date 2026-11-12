//! Long recordings on top of the replay buffer: a snapshot while recording
//! (whole-session mode) and a stop (manual mode) both produce clean files
//! with every frame in order and audio in sync.
//!
//! cargo test --release --test recording -- --ignored --nocapture

use std::time::{Duration, Instant};

use snag::audio::{AudioSource, AudioSourceFactory, BeepSource};
use snag::buffer::recording::Recording;
use snag::buffer::source::{FrameSource, SyntheticSource};
use snag::buffer::{Recorder, RecorderConfig, SourceFactory};
use snag::verify::verify_clip;

#[test]
#[ignore = "needs ffmpeg and ~15 s; run with --ignored"]
fn snapshot_and_stop_make_complete_files() {
    snag::logging::init("test-recording");
    let probes = snag::encode::detect::probe_all();
    let enc = probes.iter().find(|p| p.works).expect("an encoder");
    let root = std::env::temp_dir().join(format!("snag-rec-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (w, h) = (1280, 720);
    let source: SourceFactory = Box::new(move || Ok(Box::new(SyntheticSource::new(w, h)) as Box<dyn FrameSource>));
    let rec = Recorder::start(
        RecorderConfig {
            encoder: enc.kind,
            width: w,
            height: h,
            fps: 60,
            bitrate_kbps: 8_000,
            // A short buffer on purpose: the recording must outlive it.
            buffer_seconds: 3,
            segment_seconds: 1,
            session_dir: root.join("session"),
            gpu_upload: enc.gpu_upload,
            audio_bitrate_kbps: 160,
        },
        source,
        Some(Box::new(|| Box::new(BeepSource::default()) as Box<dyn AudioSource>) as AudioSourceFactory),
    )
    .unwrap();
    let t0 = Instant::now();
    while rec.session.start_instant().is_none() {
        assert!(t0.elapsed() < Duration::from_secs(20), "recorder didn't start: {:?}", rec.session.error());
        std::thread::sleep(Duration::from_millis(20));
    }
    // Start right away, as whole-session mode does at launch: the encoder
    // may still be writing its file header at this point.
    let recording = Recording::start(rec.session.clone()).unwrap();
    std::thread::sleep(Duration::from_secs(5));
    // Whole-session mode: save what we have so far, keep recording.
    let snap = recording.snapshot_job(&root)().unwrap();
    let r = verify_clip(&snap.path, true, None).unwrap();
    println!("snapshot: {}", r.summary());
    assert!(r.is_ok(), "{}", r.summary());
    assert!(r.frames >= 60 * 4, "snapshot has {} frames", r.frames);
    assert!(r.worst_av_offset_ms().abs() < 25.0, "{}", r.summary());

    // Longer than the 3 s buffer: segments must have been copied in time.
    std::thread::sleep(Duration::from_secs(4));
    let temp = recording.temp.clone();
    let done = recording.stop(&root, false)().unwrap();
    rec.stop();
    let r = verify_clip(&done.path, true, None).unwrap();
    println!("stopped: {}", r.summary());
    assert!(r.is_ok(), "{}", r.summary());
    assert!(r.frames >= 60 * 8, "recording has {} frames (expected ~9 s)", r.frames);
    assert!(r.worst_av_offset_ms().abs() < 25.0, "{}", r.summary());
    assert!(!temp.exists(), "the temp file is removed after saving");

    let _ = std::fs::remove_dir_all(&root);
}
