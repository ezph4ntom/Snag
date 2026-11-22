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
use std::sync::Arc;

use snag::trimmer::{EditJob, Output, Piece, TrimJob, TrimMode, effective_start, export, fast_join_problem, probe, trim};
use snag::verify::verify_clip;

/// Records a 4-second barcoded clip (60 fps, beeping sound) into `root`.
fn record_clip(root: &std::path::Path, session: &str, w: u32, h: u32) -> std::path::PathBuf {
    let probes = snag::encode::detect::probe_all();
    let enc = probes.iter().find(|p| p.works).expect("an encoder");
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
            session_dir: root.join(session),
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
    let clip = save_clip(&rec.session, Instant::now(), 4.0, root, Some("Test Game")).unwrap().path;
    rec.stop();
    clip
}

#[test]
#[ignore = "needs ffmpeg and a few seconds; run with --ignored"]
fn fast_and_precise_trims_are_frame_exact() {
    snag::logging::init("test-trimmer");
    let probes = snag::encode::detect::probe_all();
    let enc = probes.iter().find(|p| p.works).expect("an encoder");
    let root = std::env::temp_dir().join(format!("snag-trim-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);

    // 1. A 4-second source clip with frame-number barcodes.
    let clip = record_clip(&root, "session", 1280, 720);
    let original = verify_clip(&clip, true, None).unwrap();
    assert_eq!(title(&clip), "Test Game", "clips are tagged with the game");
    assert!(original.is_ok(), "{}", original.summary());
    let first = original.first_code.unwrap();
    let p = probe(&clip).unwrap();
    println!("source: {}", original.summary());
    assert_eq!(p.keyframes[..3], [0, 60, 120], "keyframe every second");

    // Preview sound: decoding from frame 60 (1 s into the recording, where a
    // beep starts) must start right at that beep, and the player must
    // actually feed the playback device.
    assert_eq!(p.audio_tracks, 1);
    {
        use std::io::Read;
        let mut child = snag::trimmer::audio::spawn_decoder(&clip, p.audio_seek_time(60)).unwrap();
        let mut bytes = vec![0u8; 48_000 / 10 * 8];
        child.stdout.take().unwrap().read_exact(&mut bytes).unwrap();
        let _ = child.kill();
        let _ = child.wait();
        let left: Vec<f32> = bytes.as_chunks::<8>().0.iter().map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        let onset = left.iter().position(|v| v.abs() > 0.05).expect("a beep in the first 100 ms");
        println!("preview audio: beep at {:.1} ms after frame 60", onset as f64 / 48.0);
        assert!(onset < 48 * 25, "preview sound starts {:.1} ms late", onset as f64 / 48.0);
        let player = snag::trimmer::audio::AudioPlayer::start(&clip, p.audio_seek_time(0), Instant::now(), 0.0);
        std::thread::sleep(Duration::from_millis(1500));
        let played = player.frames_played.load(std::sync::atomic::Ordering::Relaxed);
        player.stop_and_wait();
        println!("preview audio: {played} frames sent to the playback device in 1.5 s");
        assert!(played > 48_000, "preview player fed only {played} frames");
    }

    // Pretend the clip was saved two days ago: trims must keep that date.
    let two_days_ago = std::time::SystemTime::now() - Duration::from_secs(2 * 86_400);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&clip)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(two_days_ago))
        .unwrap();
    let mtime = |p: &std::path::Path| std::fs::metadata(p).unwrap().modified().unwrap();
    let original_mtime = mtime(&clip);

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
        assert_eq!(mtime(&out), original_mtime, "{mode:?}: the trimmed copy keeps the clip's date");
        assert_eq!(title(&out), "Test Game", "{mode:?}: the trimmed copy keeps the game tag");
    }

    // Overwrite mode replaces the original in place.
    let job = TrimJob { start: 120, end: 179, mode: TrimMode::Fast, output: Output::Overwrite, encoder: enc.kind };
    let out = trim(&p, &job, &|_| {}, &AtomicBool::new(false)).unwrap();
    assert_eq!(out, clip);
    assert_eq!(mtime(&clip), original_mtime, "overwriting keeps the clip's date");
    let r = verify_clip(&clip, true, None).unwrap();
    assert!(r.is_ok() && r.frames == 60 && r.first_code == Some(first + 120), "{}", r.summary());
    println!("overwrite: {}", r.summary());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
#[ignore = "needs ffmpeg and ~20 seconds; run with --ignored"]
fn editor_joins_pieces_and_clips() {
    snag::logging::init("test-editor");
    let probes = snag::encode::detect::probe_all();
    let enc = probes.iter().find(|p| p.works).expect("an encoder").kind;
    let root = std::env::temp_dir().join(format!("snag-edit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let a = Arc::new(probe(&record_clip(&root, "a", 1280, 720)).unwrap());
    let b = Arc::new(probe(&record_clip(&root, "b", 1280, 720)).unwrap());
    let small = Arc::new(probe(&record_clip(&root, "c", 960, 540)).unwrap());
    let first_a = verify_clip(&a.path, true, None).unwrap().first_code.unwrap();
    let first_b = verify_clip(&b.path, true, None).unwrap().first_code.unwrap();
    let piece = |p: &Arc<snag::trimmer::ClipProbe>, start, end| Piece { probe: p.clone(), start, end };
    let run = |pieces: Vec<Piece>, mode, separate| {
        let job = EditJob { pieces, mode, separate, encoder: enc };
        export(&job, &|_| {}, &std::sync::atomic::AtomicBool::new(false))
    };

    // 1. Cut a second out of the middle (keep 0-59 and 120-179), losslessly:
    //    one jump in the frame numbers, sound still in sync after it.
    let out = run(vec![piece(&a, 0, 59), piece(&a, 120, 179)], TrimMode::Fast, false).unwrap();
    assert_eq!(out.len(), 1);
    assert!(out[0].file_name().unwrap().to_string_lossy().ends_with("_edit.mp4"), "{}", out[0].display());
    let r = verify_clip(&out[0], true, None).unwrap();
    println!("cut out the middle (fast): {}", r.summary());
    assert_eq!(r.frames, 120);
    assert_eq!(r.first_code, Some(first_a));
    assert_eq!(r.gaps, vec![(60, first_a + 59, first_a + 120)]);
    assert!(r.timing_errors.is_empty() && r.duplicate_frames.is_empty() && r.unreadable_frames.is_empty(), "{}", r.summary());
    let audio = r.audio_seconds.expect("sound kept");
    assert!((audio - 2.0).abs() < 0.1, "audio {audio:.3} s");
    assert!(r.worst_av_offset_ms().abs() < 30.0, "{}", r.summary());
    assert_eq!(title(&out[0]), "Test Game", "the joined clip keeps the game tag");

    // 2. Two clips joined, B's piece first (order is the pieces' order).
    let out = run(vec![piece(&b, 60, 119), piece(&a, 0, 59)], TrimMode::Fast, false).unwrap();
    let r = verify_clip(&out[0], true, None).unwrap();
    println!("two clips (fast): {}", r.summary());
    assert_eq!(r.frames, 120);
    assert_eq!(r.first_code, Some(first_b + 60));
    assert_eq!(r.gaps, vec![(60, first_b + 119, first_a)]);
    assert!(r.worst_av_offset_ms().abs() < 30.0, "{}", r.summary());

    // 3. Clips of different sizes: Fast refuses, Precise fits the second one
    //    into the first one's 1280x720 and starts exactly on frame 75.
    let mixed = vec![piece(&a, 75, 134), piece(&small, 0, 59)];
    assert!(fast_join_problem(&mixed).is_some());
    assert!(run(mixed.clone(), TrimMode::Fast, false).is_err());
    let out = run(mixed, TrimMode::Precise, false).unwrap();
    let p = probe(&out[0]).unwrap();
    let r = verify_clip(&out[0], false, None).unwrap();
    println!("different sizes (precise): {}x{} {}", p.width, p.height, r.summary());
    assert_eq!((p.width, p.height), (1280, 720));
    assert_eq!(p.frame_count(), 120);
    assert_eq!(r.first_code, Some(first_a + 75));
    let audio = r.audio_seconds.expect("sound kept");
    assert!((audio - 2.0).abs() < 0.15, "audio {audio:.3} s");

    // 4. Each piece as its own file.
    let out = run(vec![piece(&a, 0, 29), piece(&a, 30, 59)], TrimMode::Precise, true).unwrap();
    assert_eq!(out.len(), 2);
    for (i, f) in out.iter().enumerate() {
        let r = verify_clip(f, true, None).unwrap();
        println!("separate {i}: {}", r.summary());
        assert!(r.is_ok(), "{}", r.summary());
        assert_eq!(r.frames, 30);
        assert_eq!(r.first_code, Some(first_a + 30 * i as u32));
    }
    // No temporary files left behind.
    let leftovers: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with('.'))
        .collect();
    assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    let _ = std::fs::remove_dir_all(&root);
}

/// The file's title tag (the game it came from).
fn title(path: &std::path::Path) -> String {
    let out = std::process::Command::new(snag::paths::ffprobe_exe())
        .args(["-v", "error", "-show_entries", "format_tags=title", "-of", "default=nw=1:nk=1"])
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}
