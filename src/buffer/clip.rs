//! Turning the rolling buffer into a finished clip file.
//!
//! 1. Work out which segments cover `[pressed - N s, pressed]`.
//! 2. Wait for the segment containing the moment you pressed the key to be
//!    finished (at most one segment length, ~1 s), so the clip includes it.
//! 3. Byte-concatenate `init.mp4` + those segments: a valid fragmented MP4.
//! 4. Remux (no re-encode, ~100 ms) into a regular MP4 with the index at the
//!    front (`+faststart`) so it plays and seeks everywhere, written as
//!    `.part` and renamed: a clip file is either complete or absent.
//!
//! None of this touches the recording pipeline, so saving never causes a
//! hiccup in the buffer.

use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::Session;
use super::playlist;
use crate::win::{BELOW_NORMAL_PRIORITY_CLASS, CREATE_NO_WINDOW};

#[derive(Clone, Debug)]
pub struct ClipResult {
    pub path: PathBuf,
    pub seconds: f64,
    pub segments: usize,
    pub bytes: u64,
    pub took: Duration,
    pub warnings: Vec<String>,
}

/// Saves the `seconds` before `pressed_at` from `session` into `out_dir`.
pub fn save_clip(session: &Session, pressed_at: Instant, seconds: f64, out_dir: &Path) -> Result<ClipResult> {
    let started = Instant::now();
    let stream_start = session.start_instant().context("the replay buffer hasn't started recording yet")?;
    let seg_len = session.segment_seconds as f64;
    let mut warnings = Vec::new();

    // Stream time of the key press (frame n is at n / fps; the pacer locks
    // frames to the wall clock, so wall time maps 1:1 to stream time).
    let t_press = pressed_at.saturating_duration_since(stream_start).as_secs_f64();
    let last_needed = (t_press / seg_len).floor() as u64;
    let first_needed = ((t_press - seconds).max(0.0) / seg_len).floor() as u64;

    // Wait for the segment that contains the key press to be completed.
    let deadline = Instant::now() + Duration::from_secs_f64(seg_len * 2.0 + 3.0);
    let pl = loop {
        let pl = playlist::read(&session.dir)?;
        if pl.last_seq().is_some_and(|s| s >= last_needed) || pl.ended {
            break pl;
        }
        if Instant::now() > deadline || !session.is_running() {
            warnings.push("encoder fell behind; the newest second may be missing".to_string());
            break pl;
        }
        std::thread::sleep(Duration::from_millis(25));
    };

    let segs: Vec<_> = pl
        .segments
        .iter()
        .filter(|s| s.seq >= first_needed && s.seq <= last_needed)
        .cloned()
        .collect();
    if segs.is_empty() {
        bail!("no recorded segments available yet");
    }
    if segs[0].seq > first_needed {
        // Normal right after the buffer starts: it simply isn't full yet.
        let have: f64 = segs.iter().map(|s| s.duration).sum();
        warnings.push(format!("buffer held only {have:.1} s of the requested {seconds:.0} s"));
    }
    for pair in segs.windows(2) {
        if pair[1].seq != pair[0].seq + 1 {
            bail!("segment gap between {} and {} (disk full or encoder restart?)", pair[0].seq, pair[1].seq);
        }
    }

    // 3. init + segments -> one fragmented MP4 (next to the buffer, fast disk).
    let joined = session.dir.join(format!("clip_{}.tmp.mp4", segs[0].seq));
    {
        let mut out = std::io::BufWriter::new(
            std::fs::File::create(&joined).with_context(|| format!("creating {}", joined.display()))?,
        );
        let mut copy = |name: &str| -> Result<()> {
            let bytes = std::fs::read(session.dir.join(name)).with_context(|| format!("reading buffer file {name}"))?;
            out.write_all(&bytes)?;
            Ok(())
        };
        copy(crate::encode::ffmpeg::INIT_SEGMENT)?;
        for s in &segs {
            copy(&s.file)?;
        }
        out.flush()?;
    }

    // 4. Remux to a normal MP4.
    std::fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    let final_path = crate::win::unique_path(out_dir, &format!("Clip_{}", crate::win::file_timestamp()), "mp4");
    let part = final_path.with_extension("mp4.part");
    let result = remux(&joined, &part);
    let _ = std::fs::remove_file(&joined);
    result?;
    std::fs::rename(&part, &final_path).with_context(|| format!("finalizing {}", final_path.display()))?;

    let seconds_saved: f64 = segs.iter().map(|s| s.duration).sum();
    let bytes = std::fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0);
    Ok(ClipResult {
        path: final_path,
        seconds: seconds_saved,
        segments: segs.len(),
        bytes,
        took: started.elapsed(),
        warnings,
    })
}

fn remux(input: &Path, output: &Path) -> Result<()> {
    let out = std::process::Command::new(crate::paths::ffmpeg_exe())
        .creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"])
        .arg(input)
        .args(["-map", "0", "-c", "copy", "-movflags", "+faststart", "-f", "mp4"])
        .arg(output)
        .output()
        .context("could not run ffmpeg for remux")?;
    if !out.status.success() {
        let _ = std::fs::remove_file(output);
        bail!("remux failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}
