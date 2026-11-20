//! "Capture only this app": records a test window while another window
//! covers it, then minimizes, resizes, closes and reopens it.
//!
//! Two small windows (green target, red cover on top) show in the corner of
//! the primary monitor for about 20 s. They never take focus. (They can't be
//! off-screen: Windows Graphics Capture gets no frames from a window that is
//! off every monitor.) Run with:
//!
//! ```text
//! cargo test --release --test window_capture -- --ignored --nocapture
//! ```

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use snag::buffer::source::{FrameSource, Source, WgcSource, WindowTarget};
use snag::buffer::{Recorder, RecorderConfig, SourceFactory};

const TITLE: &str = "Snag window-capture test target";

#[test]
#[ignore = "shows two small windows for ~20 s, needs a desktop session; run with --ignored"]
fn records_only_the_chosen_window() {
    snag::logging::init("test-window-capture");
    snag::encode::detect::ffmpeg_version().expect("ffmpeg must be installed (see README)");
    let probes = snag::encode::detect::probe_all();
    let enc = probes.iter().find(|p| p.works).expect("a working encoder");
    let root = std::env::temp_dir().join(format!("snag-window-capture-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let clips = root.join("clips");
    std::fs::create_dir_all(&clips).unwrap();

    let (x, y) = corner();
    let exe = std::env::current_exe().unwrap().file_name().unwrap().to_string_lossy().into_owned();
    let mut target = Some(TestWindow::open(TITLE, GREEN, x, y, Kind::Normal));
    let _cover = TestWindow::open("cover", RED, x, y, Kind::Cover);
    std::thread::sleep(Duration::from_millis(500));
    assert!(snag::capture::window::find(&exe, TITLE).is_some(), "the target window is listed");

    let t = WindowTarget { exe: exe.clone(), title: TITLE.into() };
    let source: SourceFactory = Box::new(move || Ok(Box::new(WgcSource::new(Source::Window(t), 1280, 720, false)?) as Box<dyn FrameSource>));
    let cfg = RecorderConfig {
        encoder: enc.kind,
        width: 1280,
        height: 720,
        fps: 30,
        bitrate_kbps: 6000,
        buffer_seconds: 10,
        segment_seconds: 1,
        session_dir: root.join("buffer"),
        gpu_upload: enc.gpu_upload,
        audio_bitrate_kbps: 128,
    };
    let rec = Recorder::start(cfg, source, None).expect("recorder");
    let s = rec.session.clone();
    wait_for("recording to start", 10, || s.start_instant().is_some());
    std::thread::sleep(Duration::from_secs(3));

    // 1. Covered by a red window: the clip still shows the green target.
    assert!(!s.waiting.load(Ordering::Relaxed));
    let c1 = last_frame_color(&clip(&s, &clips, "1-covered"));
    println!("covered: {c1:?}");
    assert!(is_green(c1), "covered window: expected green, got {c1:?}");

    // 2. Minimized: black, and the session says it's waiting.
    target.as_ref().unwrap().send(Cmd::Minimize);
    wait_for("waiting (minimized)", 5, || s.waiting.load(Ordering::Relaxed));
    std::thread::sleep(Duration::from_secs(2));
    let c2 = last_frame_color(&clip(&s, &clips, "2-minimized"));
    println!("minimized: {c2:?}");
    assert!(is_black(c2), "minimized: expected black, got {c2:?}");

    // 3. Restored: green again.
    target.as_ref().unwrap().send(Cmd::Restore);
    wait_for("capturing again (restored)", 5, || !s.waiting.load(Ordering::Relaxed));
    std::thread::sleep(Duration::from_secs(2));
    let c3 = last_frame_color(&clip(&s, &clips, "3-restored"));
    println!("restored: {c3:?}");
    assert!(is_green(c3), "restored: expected green, got {c3:?}");

    // 3b. Resized to a square: stretched to fill the same 1280x720 clip
    // (all green, no black bars), nothing restarts.
    target.as_ref().unwrap().send(Cmd::Resize(H, H));
    std::thread::sleep(Duration::from_secs(2));
    let c3b = last_frame_color(&clip(&s, &clips, "3b-resized"));
    println!("resized: {c3b:?}");
    assert!(is_green(c3b), "resized: expected all green (stretched, no bars), got {c3b:?}");
    target.as_ref().unwrap().send(Cmd::Resize(W, H));

    // 4. Closed: black and waiting.
    drop(target.take());
    wait_for("waiting (closed)", 5, || s.waiting.load(Ordering::Relaxed));
    std::thread::sleep(Duration::from_secs(2));
    let c4 = last_frame_color(&clip(&s, &clips, "4-closed"));
    println!("closed: {c4:?}");
    assert!(is_black(c4), "closed: expected black, got {c4:?}");

    // 5. Reopened: found again by exe name (looked for every 2 s).
    target = Some(TestWindow::open(TITLE, GREEN, x, y, Kind::Normal));
    wait_for("re-attach (reopened)", 8, || !s.waiting.load(Ordering::Relaxed));
    std::thread::sleep(Duration::from_secs(2));
    let c5 = last_frame_color(&clip(&s, &clips, "5-reopened"));
    println!("reopened: {c5:?}");
    assert!(is_green(c5), "reopened: expected green, got {c5:?}");

    assert!(s.is_running(), "recorder error: {:?}", s.error());
    rec.stop();
    drop(target);
    println!("clips in {}", clips.display());
}
