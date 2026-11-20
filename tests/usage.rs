//! Resource usage of the recording pipeline: records 1080p60 like the
//! Quality preset (NVENC if available, 18.5 Mbps) for `SNAG_USAGE_SECS`
//! seconds (default 60) and prints the frame counters. CPU, memory, GPU
//! and handle numbers are read from outside while it runs (see
//! docs/DEVELOPING.md > Tests). Captures the primary monitor, or a small
//! animated test window in its corner. `SNAG_USAGE_PREVIEW=1` also makes
//! the Live tab's preview frames (only while Snag itself isn't running).
//! Run with:
//!
//! ```text
//! cargo test --release --test usage -- --ignored --nocapture --exact record_monitor
//! cargo test --release --test usage -- --ignored --nocapture --exact record_window
//! ```

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use snag::buffer::source::{FrameSource, Source, WgcSource, WindowTarget};
use snag::buffer::{Recorder, RecorderConfig, SourceFactory};

const TITLE: &str = "Snag usage test window";

fn record(name: &str, source: Source) {
    snag::logging::init("test-usage");
    snag::encode::detect::ffmpeg_version().expect("ffmpeg must be installed (see README)");
    let probes = snag::encode::detect::probe_all();
    let enc = probes.iter().find(|p| p.works).expect("a working encoder");
    let secs: u64 = std::env::var("SNAG_USAGE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(60);
    let root = std::env::temp_dir().join(format!("snag-usage-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let factory: SourceFactory = Box::new(move || Ok(Box::new(WgcSource::new(source, 1920, 1080, true)?) as Box<dyn FrameSource>));
    let cfg = RecorderConfig {
        encoder: enc.kind,
        width: 1920,
        height: 1080,
        fps: 60,
        bitrate_kbps: 18_500,
        buffer_seconds: 120,
        segment_seconds: 1,
        session_dir: root.join("buffer"),
        gpu_upload: enc.gpu_upload,
        audio_bitrate_kbps: 160,
    };
    // SNAG_USAGE_PREVIEW=1: also make live-preview frames, as while the Live
    // tab is open. They go to Snag's real preview block: don't use it while
    // Snag itself runs.
    if std::env::var_os("SNAG_USAGE_PREVIEW").is_some() {
        snag::capture::preview::WANTED.store(true, Ordering::Relaxed);
    }
    let rec = Recorder::start(cfg, factory, None).expect("recorder");
    let s = rec.session.clone();
    wait_for("recording to start", 10, || s.start_instant().is_some());
    println!("USAGE-START {name} pid {} encoder {:?}", std::process::id(), enc.kind);
    std::thread::sleep(Duration::from_secs(secs));
    let st = &s.stats;
    println!(
        "USAGE-END {name}: {} frames encoded, {} dropped, {} unchanged-screen repeats in {secs} s",
        st.frames_encoded.load(Ordering::Relaxed),
        st.frames_dropped.load(Ordering::Relaxed),
        st.frames_unchanged.load(Ordering::Relaxed)
    );
    assert!(s.is_running(), "recorder error: {:?}", s.error());
    rec.stop();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
#[ignore = "records the primary monitor for a minute; run with --ignored"]
fn record_monitor() {
    let m = snag::capture::primary_monitor().expect("a monitor");
    record("monitor", Source::Monitor(m.handle));
}

#[test]
#[ignore = "records a small animated window for a minute; run with --ignored"]
fn record_window() {
    let (x, y) = corner();
    let w = TestWindow::open(TITLE, GREEN, x, y, Kind::Normal);
    w.send(Cmd::Animate);
    // SNAG_USAGE_SQUARE=1: a square window (shows the stretch to 16:9).
    if std::env::var_os("SNAG_USAGE_SQUARE").is_some() {
        w.send(Cmd::Resize(H, H));
    }
    let exe = std::env::current_exe().unwrap().file_name().unwrap().to_string_lossy().into_owned();
    record("window", Source::Window(WindowTarget { exe, title: TITLE.into() }));
    drop(w);
}
