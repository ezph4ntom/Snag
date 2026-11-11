//! The trimmer's non-UI half: inspect a clip, decode preview frames, and
//! export the trimmed result.
//!
//! * **Fast** mode copies the compressed video untouched (lossless, takes a
//!   fraction of a second). The cut can only *start* on a keyframe, because
//!   frames after a keyframe are stored as differences from it; the end can
//!   be any frame. Snag records a keyframe every second, so the start
//!   snaps to the nearest second at or before your handle.
//! * **Precise** mode re-encodes, so it can start on any frame. Slower and
//!   very slightly lossy (high bitrate keeps it visually identical).

pub mod audio;
pub mod player;

use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};

use crate::encode::EncoderKind;
use crate::win::{BELOW_NORMAL_PRIORITY_CLASS, CREATE_NO_WINDOW};

/// What we know about a clip after one quick `ffprobe` pass (no decoding).
#[derive(Clone, Debug)]
pub struct ClipProbe {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// Presentation time of each frame in seconds, relative to the first
    /// frame, in display order.
    pub times: Vec<f64>,
    /// Indices into `times` of keyframes (ascending).
    pub keyframes: Vec<usize>,
    pub bytes: u64,
    /// Seconds from the start of the *file* to the first video frame.
    /// ffmpeg's `-ss` counts from the file start, which is the earliest
    /// packet of any stream; with audio that's often a few ms before the
    /// first video frame, and ignoring it would cut one frame early.
    pub video_offset: f64,
    /// Number of audio tracks.
    pub audio_tracks: usize,
}

impl ClipProbe {
    pub fn frame_count(&self) -> usize {
        self.times.len()
    }

    pub fn time_of(&self, frame: usize) -> f64 {
        self.times.get(frame).copied().unwrap_or_else(|| self.duration())
    }

    pub fn frame_duration(&self) -> f64 {
        if self.fps > 0.0 { 1.0 / self.fps } else { 1.0 / 60.0 }
    }

    pub fn duration(&self) -> f64 {
        self.times.last().map(|t| t + self.frame_duration()).unwrap_or(0.0)
    }

    /// Frame displayed at time `t`.
    pub fn frame_at(&self, t: f64) -> usize {
        match self.times.binary_search_by(|x| x.total_cmp(&t)) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        }
        .min(self.frame_count().saturating_sub(1))
    }

    /// The last keyframe at or before `frame` (0 if none).
    pub fn keyframe_at_or_before(&self, frame: usize) -> usize {
        match self.keyframes.binary_search(&frame) {
            Ok(i) => self.keyframes[i],
            Err(0) => 0,
            Err(i) => self.keyframes[i - 1],
        }
    }

    /// `-ss` value that lands on `frame` (plus `bias` seconds).
    pub fn seek_time(&self, frame: usize, bias: f64) -> f64 {
        (self.video_offset + self.time_of(frame) + bias).max(0.0)
    }

    /// File time of `frame`, for decoding the sound that goes with it.
    pub fn audio_seek_time(&self, frame: usize) -> f64 {
        self.video_offset + self.time_of(frame)
    }

    pub fn bitrate_kbps(&self) -> u32 {
        let d = self.duration().max(0.1);
        (self.bytes as f64 * 8.0 / d / 1000.0) as u32
    }
}

/// Reads frame timestamps, keyframe flags, size and frame rate.
pub fn probe(path: &Path) -> Result<ClipProbe> {
    let out = crate::win::hidden_command(crate::paths::ffprobe_exe())
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=width,height,r_frame_rate,avg_frame_rate:packet=pts_time,flags:format=start_time"])
        .args(["-of", "json"])
        .arg(path)
        .output()
        .with_context(|| format!("could not run {}", crate::paths::ffprobe_exe().display()))?;
    if !out.status.success() {
        bail!("not a readable video: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).context("ffprobe output")?;
    let stream = json["streams"].get(0).context("no video stream")?;
    let rate = |key: &str| -> f64 {
        let s = stream[key].as_str().unwrap_or("0/1");
        let mut it = s.split('/');
        let n: f64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
        let d: f64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(1.0);
        if d > 0.0 { n / d } else { 0.0 }
    };
    let fps = match rate("avg_frame_rate") {
        f if f > 0.0 => f,
        _ => rate("r_frame_rate"),
    };

    // Packets come in decode order; sort by time to get display order
    // (identical for our clips, which have no B-frames).
    let mut packets: Vec<(f64, bool)> = json["packets"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    let t: f64 = p["pts_time"].as_str()?.parse().ok()?;
                    Some((t, p["flags"].as_str().unwrap_or("").contains('K')))
                })
                .collect()
        })
        .unwrap_or_default();
    if packets.is_empty() {
        bail!("the file has no video frames");
    }
    packets.sort_by(|a, b| a.0.total_cmp(&b.0));
    let t0 = packets[0].0;
    let file_start: f64 = json["format"]["start_time"].as_str().and_then(|v| v.parse().ok()).unwrap_or(t0);
    let times = packets.iter().map(|p| p.0 - t0).collect();
    let mut keyframes: Vec<usize> = packets.iter().enumerate().filter(|(_, p)| p.1).map(|(i, _)| i).collect();
    if keyframes.first() != Some(&0) {
        keyframes.insert(0, 0);
    }
    let audio_tracks = crate::win::hidden_command(crate::paths::ffprobe_exe())
        .args(["-v", "error", "-select_streams", "a", "-show_entries", "stream=index", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0);
    Ok(ClipProbe {
        path: path.to_path_buf(),
        width: stream["width"].as_u64().unwrap_or(0) as u32,
        height: stream["height"].as_u64().unwrap_or(0) as u32,
        fps,
        times,
        keyframes,
        bytes: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        video_offset: (t0 - file_start).max(0.0),
        audio_tracks,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrimMode {
    Fast,
    Precise,
}

#[derive(Clone, Debug)]
pub enum Output {
    /// `<name>_trim.mp4` next to the original (or `_trim_2`...).
    NewFile,
    /// Replace the original file (written to a temp file first).
    Overwrite,
}

#[derive(Clone, Debug)]
pub struct TrimJob {
    pub start: usize,
    /// Inclusive.
    pub end: usize,
    pub mode: TrimMode,
    pub output: Output,
    /// Encoder for precise mode.
    pub encoder: EncoderKind,
}

/// First frame that will actually be in the output.
pub fn effective_start(probe: &ClipProbe, start: usize, mode: TrimMode) -> usize {
    match mode {
        TrimMode::Fast => probe.keyframe_at_or_before(start),
        TrimMode::Precise => start,
    }
}

/// Runs the export. `progress` gets 0..=1. Set `cancel` to abort.
pub fn trim(probe: &ClipProbe, job: &TrimJob, progress: &dyn Fn(f32), cancel: &AtomicBool) -> Result<PathBuf> {
    let n = probe.frame_count();
    if n == 0 || job.start > job.end || job.end >= n {
        bail!("invalid trim range {}..={} of {n} frames", job.start, job.end);
    }
    let src = &probe.path;
    let dir = src.parent().unwrap_or(Path::new("."));
    let stem = src.file_stem().unwrap_or_default().to_string_lossy();
    let final_path = match job.output {
        Output::NewFile => crate::win::unique_path(dir, &format!("{stem}_trim"), "mp4"),
        Output::Overwrite => src.clone(),
    };
    let part = dir.join(format!(".{stem}.trim.part.mp4"));

    let start = effective_start(probe, job.start, job.mode);
    let frames = job.end - start + 1;
    let half = probe.frame_duration() / 2.0;
    let mut cmd = std::process::Command::new(crate::paths::ffmpeg_exe());
    cmd.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-progress", "pipe:1", "-nostats"]);
    match job.mode {
        TrimMode::Fast => {
            // Seek target just after the keyframe: input seeking with stream
            // copy starts at the keyframe at/before it, i.e. exactly ours
            // (aiming at the keyframe itself could round to the previous one).
            cmd.args(["-ss", &format!("{:.6}", probe.seek_time(start, half))])
                .arg("-i")
                .arg(src)
                .args(["-map", "0", "-c", "copy", "-avoid_negative_ts", "make_zero"]);
        }
        TrimMode::Precise => {
            // Accurate seek decodes from the keyframe and drops frames before
            // the target; half a frame early so rounding can't skip `start`.
            let fps = probe.fps.round().max(1.0) as u32;
            let kbps = (probe.bitrate_kbps() as f64 * 1.25).max(8000.0) as u32;
            cmd.args(["-ss", &format!("{:.6}", probe.seek_time(start, -half))])
                .arg("-i")
                .arg(src)
                .args(["-map", "0:v:0", "-map", "0:a?", "-c:a", "aac", "-b:a", "160k"])
                .args(crate::encode::ffmpeg::video_codec_args(job.encoder, fps, kbps, 1, false));
        }
    }
    cmd.args(["-frames:v", &frames.to_string(), "-movflags", "+faststart", "-f", "mp4"])
        .arg(&part)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    log::info!(
        "trim {:?}: {} frames {start}..={} of {} -> {}",
        job.mode,
        frames,
        job.end,
        src.display(),
        final_path.display()
    );
    let mut child = cmd.spawn().context("could not run ffmpeg")?;
    let stdout = child.stdout.take().context("no stdout")?;
    for line in BufReader::new(stdout).lines().map_while(|l| l.ok()) {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&part);
            bail!("cancelled");
        }
        if let Some(f) = line.strip_prefix("frame=").and_then(|v| v.trim().parse::<f32>().ok()) {
            progress((f / frames as f32).min(1.0));
        }
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        let _ = std::fs::remove_file(&part);
        bail!("ffmpeg failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }

    // Replace/rename. Explorer or a video player can briefly hold the
    // original open, so retry a few times before giving up.
    let mut last_err = None;
    for _ in 0..20 {
        match std::fs::rename(&part, &final_path) {
            Ok(()) => {
                progress(1.0);
                return Ok(final_path);
            }
            Err(e) => last_err = Some(e),
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = std::fs::remove_file(&part);
    Err(last_err.unwrap()).with_context(|| format!("could not write {} (is it open in another program?)", final_path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe_with(keys: Vec<usize>, n: usize) -> ClipProbe {
        ClipProbe {
            path: PathBuf::new(),
            width: 1920,
            height: 1080,
            fps: 60.0,
            times: (0..n).map(|i| i as f64 / 60.0).collect(),
            keyframes: keys,
            bytes: 0,
            video_offset: 0.0,
            audio_tracks: 1,
        }
    }

    #[test]
    fn keyframe_snapping() {
        let p = probe_with(vec![0, 60, 120], 180);
        assert_eq!(p.keyframe_at_or_before(0), 0);
        assert_eq!(p.keyframe_at_or_before(59), 0);
        assert_eq!(p.keyframe_at_or_before(60), 60);
        assert_eq!(p.keyframe_at_or_before(179), 120);
        assert_eq!(effective_start(&p, 75, TrimMode::Fast), 60);
        assert_eq!(effective_start(&p, 75, TrimMode::Precise), 75);
        assert_eq!(p.frame_at(1.0), 60);
        assert_eq!(p.frame_at(1.009), 60);
        assert_eq!(p.frame_at(99.0), 179);
        assert!((p.duration() - 3.0).abs() < 1e-9);
    }
}
