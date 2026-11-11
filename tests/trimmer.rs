//! Trimmer exactness: records a short barcoded synthetic clip, trims it in
//! both modes, and reads the frame numbers back from the outputs.
//!
//! cargo test --release --test trimmer -- --ignored --nocapture

use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use snag::audio::{AudioSource, AudioSourceFactory, BeepSource};
use snag::buffer::clip::save_clip;
use snag::buffer::source::{FrameSource, SyntheticSource};
use snag::buffer::{Recorder, RecorderConfig, SourceFactory};
use snag::trimmer::{Output, TrimJob, TrimMode, effective_start, probe, trim};
use snag::verify::verify_clip;

#[test]
#[ignore = "needs ffmpeg and a few seconds; run with --ignored"]
fn fast_and_precise_trims_are_frame_exact() {
    snag::logging::init("test-trimmer");
    let probes = snag::encode::detect::probe_all();
    let enc = probes.iter().find(|p| p.works).expect("an encoder");
    let root = std::env::temp_dir().join(format!("snag-trim-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);

    // 1. A 4-second source clip with frame-number barcodes.
    let (w, h) = (1280, 720);
    let source: SourceFactory = Box::new(move || Ok(Box::new(SyntheticSource::new(w, h)) as Box<dyn FrameSource>));
    let rec = Recorder::start(
        RecorderConfig {
            encoder: enc.kind,
            width: w,
            height: h,
            fps: 60,
            bitrate_kbps: 10_000,
            buffer_seconds: 4,
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
    while rec.session.buffered_seconds(4) < 4.0 {
        assert!(t0.elapsed() < Duration::from_secs(20), "recorder didn't start: {:?}", rec.session.error());
        std::thread::sleep(Duration::from_millis(50));
    }
    let clip = save_clip(&rec.session, Instant::now(), 4.0, &root).unwrap().path;
    rec.stop();
    let original = verify_clip(&clip, true, None).unwrap();
    assert!(original.is_ok(), "{}", original.summary());
    let first = original.first_code.unwrap();
    let p = probe(&clip).unwrap();
    println!("source: {}", original.summary());
    assert_eq!(p.keyframes[..3], [0, 60, 120], "keyframe every second");

    let (start, end) = (75usize, 200usize);
    for mode in [TrimMode::Precise, TrimMode::Fast] {
        let job = TrimJob { start, end, mode, output: Output::NewFile, encoder: enc.kind };
        let out = trim(&p, &job, &|_| {}, &AtomicBool::new(false)).unwrap();
        let r = verify_clip(&out, true, None).unwrap();
        println!("{mode:?}: {}", r.summary());
        let eff = effective_start(&p, start, mode);
        assert!(r.is_ok(), "{mode:?}: {}", r.summary());
        assert_eq!(r.frames, end - eff + 1, "{mode:?} frame count");
        assert_eq!(r.first_code, Some(first + eff as u32), "{mode:?} first frame");
        // Audio survives the trim and stays in sync.
        let a = r.audio_seconds.expect("trimmed clip keeps its audio");
        assert!((a - r.frames as f64 / 60.0).abs() < 0.1, "{mode:?}: audio {a:.3} s");
        assert!(r.worst_av_offset_ms().abs() < 25.0, "{mode:?}: {}", r.summary());
        if mode == TrimMode::Fast {
            assert_eq!(eff, 60, "fast mode starts on the keyframe before 75");
        }
    }

    // Overwrite mode replaces the original in place.
    let job = TrimJob { start: 120, end: 179, mode: TrimMode::Fast, output: Output::Overwrite, encoder: enc.kind };
    let out = trim(&p, &job, &|_| {}, &AtomicBool::new(false)).unwrap();
    assert_eq!(out, clip);
    let r = verify_clip(&clip, true, None).unwrap();
    assert!(r.is_ok() && r.frames == 60 && r.first_code == Some(first + 120), "{}", r.summary());
    println!("overwrite: {}", r.summary());
    let _ = std::fs::remove_dir_all(&root);
}
