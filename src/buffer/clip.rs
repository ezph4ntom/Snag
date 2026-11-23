//! Turning the rolling buffer into a finished clip file.
//!
//! 1. Work out which segments cover `[pressed - N s, pressed]`.
//! 2. Wait for the segment containing the moment you pressed the key to be
//!    finished (at most one segment length, ~1 s), so the clip includes it.
//! 3. `init.mp4` + those segments, byte-concatenated, are a valid fragmented
//!    MP4: ffmpeg reads them that way straight from the buffer (`concat:`).
//! 4. Remux (no re-encode, ~0.3 s for a minute) into a regular MP4 with the
//!    index at the front (`+faststart`) so it plays and seeks everywhere,
//!    written as `.part` and renamed: a clip file is either complete or absent.
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
/// `app`: the game/program the clip came from, stored as the file's title.
pub fn save_clip(session: &Session, pressed_at: Instant, seconds: f64, out_dir: &Path, app: Option<&str>) -> Result<ClipResult> {
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

    // 3 + 4. init + segments, read by ffmpeg as one fragmented MP4, remuxed
    // into a normal MP4.
    std::fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    let final_path = crate::win::unique_path(out_dir, &format!("Clip_{}", crate::win::file_timestamp()), "mp4");
    let part = std::path::absolute(final_path.with_extension("mp4.part"))?;
    let names: Vec<&str> = std::iter::once(crate::encode::ffmpeg::INIT_SEGMENT)
        .chain(segs.iter().map(|s| s.file.as_str()))
        .collect();
    let direct = concat_url(&names).map(|url| remux(std::ffi::OsStr::new(&url), &session.dir, &part, app));
    match direct {
        Some(Ok(())) => {}
        Some(Err(e)) => {
            log::warn!("saving straight from the buffer failed ({e:#}); joining the segments first");
            remux_joined(session, &names, segs[0].seq, &part, app)?;
        }
        None => remux_joined(session, &names, segs[0].seq, &part, app)?,
    }
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

/// ffmpeg arguments that tag a saved file: the app it came from (as the
/// title, which the clip library shows) and when it was made.
pub fn tag_args(app: Option<&str>) -> Vec<String> {
    let mut a = vec!["-metadata".to_string(), "creation_time=now".to_string()];
    if let Some(app) = app.map(str::trim).filter(|s| !s.is_empty()) {
        a.push("-metadata".into());
        a.push(format!("title={app}"));
    }
    a
}

/// Most files ffmpeg's `concat:` protocol may open at once: it opens every
/// part up front, and the C runtime allows 512 open files per process.
const MAX_CONCAT_FILES: usize = 400;

/// `concat:init.mp4|seg_000001.m4s|...`: ffmpeg reads the buffer files
/// (relative to the session folder) back to back as one file, which is the
/// fragmented MP4 we want, without writing a joined copy to disk first
/// (half the disk writes of a save). None when there are too many files,
/// or the command line would get too long.
fn concat_url(names: &[&str]) -> Option<String> {
    let url = format!("concat:{}", names.join("|"));
    (names.len() <= MAX_CONCAT_FILES && url.len() < 30_000 && !names.iter().any(|n| n.contains('|'))).then_some(url)
}

/// The fallback: byte-concatenate the files into one temp file next to the
/// buffer, then remux that.
fn remux_joined(session: &Session, names: &[&str], first_seq: u64, part: &Path, app: Option<&str>) -> Result<()> {
    let joined = session.dir.join(format!("clip_{first_seq}.tmp.mp4"));
    let written = (|| -> Result<()> {
        let mut out = std::io::BufWriter::new(
            std::fs::File::create(&joined).with_context(|| format!("creating {}", joined.display()))?,
        );
        for name in names {
            let bytes = std::fs::read(session.dir.join(name)).with_context(|| format!("reading buffer file {name}"))?;
            out.write_all(&bytes)?;
        }
        out.flush()?;
        Ok(())
    })();
    let result = written.and_then(|()| remux(joined.as_os_str(), &session.dir, part, app));
    let _ = std::fs::remove_file(&joined);
    result
}

/// Remuxes `input` (a file, or an ffmpeg URL relative to `dir`) to `output`
/// (an absolute path).
fn remux(input: &std::ffi::OsStr, dir: &Path, output: &Path, app: Option<&str>) -> Result<()> {
    let out = std::process::Command::new(crate::paths::ffmpeg_exe())
        .creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS)
        .current_dir(dir)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"])
        .arg(input)
        .args(["-map", "0", "-c", "copy"])
        .args(tag_args(app))
        .args(["-movflags", "+faststart", "-f", "mp4"])
        .arg(output)
        .output()
        .context("could not run ffmpeg for remux")?;
    if !out.status.success() {
        let _ = std::fs::remove_file(output);
        bail!("remux failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{MAX_CONCAT_FILES, concat_url};

    #[test]
    fn concat_url_lists_the_buffer_files_in_order() {
        assert_eq!(concat_url(&["init.mp4", "seg_000007.m4s", "seg_000008.m4s"]).unwrap(), "concat:init.mp4|seg_000007.m4s|seg_000008.m4s");
        // A 15-minute buffer (the maximum) is too many files at once: the
        // caller joins them on disk instead.
        let names: Vec<String> = (0..901).map(|i| format!("seg_{i:06}.m4s")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        assert!(concat_url(&names).is_none());
        assert!(concat_url(&names[..MAX_CONCAT_FILES]).is_some());
        assert!(concat_url(&["init.mp4", "a|b.m4s"]).is_none());
    }
}
