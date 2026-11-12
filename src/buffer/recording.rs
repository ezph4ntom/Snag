//! Long recordings ("Start recording" / "record the whole session"),
//! built on the replay buffer's own encoder: no second encoder runs.
//!
//! The buffer's encoder already writes the stream as 1-second fragmented
//! MP4 segments. While a recording runs, a thread watches the buffer's
//! playlist and appends each finished segment to one growing file
//! (`init.mp4` + segments = a valid fragmented MP4). That costs a file copy
//! of ~2 MB/s and nothing else, and the file is playable at every moment,
//! so a crash loses at most the last second (leftovers are recovered when
//! Snag starts again: see `recover_leftovers`).
//!
//! * `stop()` appends the last segment and remuxes the file into a regular
//!   MP4 in the clips folder.
//! * `save_snapshot()` (whole-session mode) remuxes everything recorded so
//!   far into a new file while recording continues: ffmpeg reads exactly
//!   the bytes written so far (`subfile:` protocol), never the half-written
//!   tail.

use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::clip::ClipResult;
use super::{Session, playlist};
use crate::win::{BELOW_NORMAL_PRIORITY_CLASS, CREATE_NO_WINDOW};

/// State shared with the appending thread.
struct Shared {
    stop: AtomicBool,
    /// Bytes of the temp file that hold complete segments (a snapshot may
    /// read up to here).
    complete_bytes: AtomicU64,
    /// Seconds of video appended so far.
    seconds_millis: AtomicU64,
}

pub struct Recording {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<Result<()>>>,
    pub temp: PathBuf,
    pub started: Instant,
}

impl Recording {
    /// Starts recording `session` from now. The growing file lives in
    /// `paths::recordings_dir()` until it is saved.
    pub fn start(session: Arc<Session>) -> Result<Recording> {
        let stream_start = session.start_instant().context("the replay buffer hasn't started yet")?;
        let dir = crate::paths::recordings_dir();
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let temp = crate::win::unique_path(&dir, &format!("Recording_{}", crate::win::file_timestamp()), "mp4");
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            complete_bytes: AtomicU64::new(0),
            seconds_millis: AtomicU64::new(0),
        });
        let (sh, path) = (shared.clone(), temp.clone());
        // The segment being written right now is the first one recorded.
        let seg_len = session.segment_seconds.max(1) as f64;
        let first = (stream_start.elapsed().as_secs_f64() / seg_len).floor() as u64;
        let thread = std::thread::Builder::new()
            .name("recording".into())
            .spawn(move || append_main(&session, &path, first, &sh))?;
        log::info!("recording started: {}", temp.display());
        Ok(Recording { shared, thread: Some(thread), temp, started: Instant::now() })
    }

    /// Seconds of video in the file so far.
    pub fn seconds(&self) -> f64 {
        self.shared.seconds_millis.load(Ordering::Relaxed) as f64 / 1000.0
    }

    pub fn bytes(&self) -> u64 {
        self.shared.complete_bytes.load(Ordering::Relaxed)
    }

    /// Whether the appending thread has ended (the buffer stopped or failed).
    pub fn ended(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }

    /// Saves everything recorded so far as a new file in `out_dir`; the
    /// recording keeps going. Blocks for the remux (call off the UI path).
    pub fn snapshot_job(&self, out_dir: &Path) -> impl FnOnce() -> Result<ClipResult> + Send + 'static {
        let (temp, bytes, seconds, out) = (self.temp.clone(), self.bytes(), self.seconds(), out_dir.to_path_buf());
        move || {
            let started = Instant::now();
            if bytes == 0 {
                bail!("nothing recorded yet");
            }
            let final_path = remux_to(&format!("subfile,,start,0,end,{bytes},,:{}", temp.display()), &out)?;
            Ok(result(final_path, seconds, started))
        }
    }

    /// Stops recording and returns a job that finishes the file into
    /// `out_dir` (appends the last segment, remuxes, deletes the temp file).
    /// `discard`: delete it instead (whole-session mode when Snag quits).
    pub fn stop(mut self, out_dir: &Path, discard: bool) -> impl FnOnce() -> Result<ClipResult> + Send + 'static {
        self.shared.stop.store(true, Ordering::Relaxed);
        let thread = self.thread.take();
        let (temp, shared, out) = (self.temp.clone(), self.shared.clone(), out_dir.to_path_buf());
        move || {
            let started = Instant::now();
            if let Some(t) = thread {
                match t.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => log::warn!("recording: {e:#}"),
                    Err(_) => log::error!("recording thread panicked"),
                }
            }
            let seconds = shared.seconds_millis.load(Ordering::Relaxed) as f64 / 1000.0;
            if discard || shared.complete_bytes.load(Ordering::Relaxed) == 0 {
                let _ = std::fs::remove_file(&temp);
                bail!("recording discarded");
            }
            let final_path = remux_to(&temp.display().to_string(), &out)?;
            let _ = std::fs::remove_file(&temp);
            Ok(result(final_path, seconds, started))
        }
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        // Dropped without stop(): let the thread end; the temp file stays and
        // is recovered on the next start.
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

fn result(path: PathBuf, seconds: f64, started: Instant) -> ClipResult {
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    ClipResult { path, seconds, segments: 0, bytes, took: started.elapsed(), warnings: Vec::new() }
}

/// Appends finished segments `first..` to `path` until stopped (then up to
/// the segment that was being written when stop was requested).
fn append_main(session: &Session, path: &Path, first: u64, shared: &Shared) -> Result<()> {
    let mut out = std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    // The init segment (file header) is copied once the first media segment
    // exists: right after the encoder starts, ffmpeg may still be writing it.
    let mut written: u64 = 0;
    let mut next = first;
    let mut seconds = 0.0f64;
    let seg_len = session.segment_seconds.max(1) as f64;
    let mut last_needed: Option<u64> = None;
    loop {
        if last_needed.is_none() && shared.stop.load(Ordering::Relaxed) {
            // Include the segment being written at the moment of the stop.
            let now = session.start_instant().map(|s| s.elapsed().as_secs_f64()).unwrap_or(0.0);
            last_needed = Some((now / seg_len).floor() as u64);
        }
        let pl = playlist::read(&session.dir)?;
        let from = next;
        for seg in pl.segments.iter().filter(|s| s.seq >= from) {
            if seg.seq != next {
                log::warn!("recording: segments {next}..{} were already gone; skipping them", seg.seq);
            }
            if written == 0 {
                let init = std::fs::read(session.dir.join(crate::encode::ffmpeg::INIT_SEGMENT)).context("reading init segment")?;
                out.write_all(&init)?;
                written = init.len() as u64;
            }
            let bytes = match std::fs::read(session.dir.join(&seg.file)) {
                Ok(b) => b,
                Err(e) => {
                    log::warn!("recording: segment {} unreadable ({e}); skipping it", seg.seq);
                    next = seg.seq + 1;
                    continue;
                }
            };
            out.write_all(&bytes)?;
            written += bytes.len() as u64;
            seconds += seg.duration;
            next = seg.seq + 1;
            shared.complete_bytes.store(written, Ordering::Relaxed);
            shared.seconds_millis.store((seconds * 1000.0) as u64, Ordering::Relaxed);
        }
        let done = match last_needed {
            Some(l) => next > l || pl.ended,
            None => false,
        };
        if done || !session.is_running() {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    out.flush()?;
    log::info!("recording: {:.0} s appended, {:.1} MB", seconds, written as f64 / 1e6);
    Ok(())
}

/// Remuxes `input` (a path or ffmpeg URL) into `out_dir/Recording_<time>.mp4`.
fn remux_to(input: &str, out_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    let final_path = crate::win::unique_path(out_dir, &format!("Recording_{}", crate::win::file_timestamp()), "mp4");
    let part = final_path.with_extension("mp4.part");
    let out = std::process::Command::new(crate::paths::ffmpeg_exe())
        .creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i", input])
        .args(["-map", "0", "-c", "copy", "-avoid_negative_ts", "make_zero", "-movflags", "+faststart", "-f", "mp4"])
        .arg(&part)
        .output()
        .context("could not run ffmpeg to save the recording")?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&part);
        bail!("saving the recording failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    std::fs::rename(&part, &final_path).with_context(|| format!("finalizing {}", final_path.display()))?;
    Ok(final_path)
}

/// Recordings left behind by a crash or power loss: remux each into
/// `out_dir` (they're playable up to their last full segment) and delete
/// the temp file. Returns the saved paths.
pub fn recover_leftovers(out_dir: &Path) -> Vec<PathBuf> {
    let mut saved = Vec::new();
    let Ok(entries) = std::fs::read_dir(crate::paths::recordings_dir()) else { return saved };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "mp4") {
            match remux_to(&p.display().to_string(), out_dir) {
                Ok(f) => {
                    log::info!("recovered recording {} -> {}", p.display(), f.display());
                    let _ = std::fs::remove_file(&p);
                    saved.push(f);
                }
                Err(e) => log::warn!("couldn't recover {}: {e:#}", p.display()),
            }
        }
    }
    saved
}
