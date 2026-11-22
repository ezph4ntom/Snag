//! The editor's non-UI half: inspect a clip, decode preview frames, and
//! export the result: one trimmed piece, several pieces joined into one
//! clip (from one clip or several), or each piece as its own file.
//!
//! * **Fast** mode copies the compressed video untouched (lossless, takes a
//!   fraction of a second). A cut can only *start* on a keyframe, because
//!   frames after a keyframe are stored as differences from it; the end can
//!   be any frame. Snag records a keyframe every second, so a start snaps
//!   to the nearest second at or before your handle. Joining copies too, so
//!   the clips must share their video size, frame rate and sound format.
//! * **Precise** mode re-encodes, so it can start on any frame and join any
//!   clips (they're fitted to the first one's size and frame rate). Slower
//!   and very slightly lossy (high bitrate keeps it visually identical).

pub mod audio;
pub mod player;

use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
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
    /// Codec, pixel format, profile and level of the video ("h264 yuv420p
    /// High 42"); with the size and frame rate, what a lossless join needs
    /// to be equal.
    pub video_format: String,
    /// Each audio track's codec, sample rate and channels ("aac 48000 2").
    pub audio_formats: Vec<String>,
    /// The file's title tag (the game a Snag clip came from).
    pub title: Option<String>,
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
        .args([
            "-show_entries",
            "stream=width,height,r_frame_rate,avg_frame_rate,codec_name,pix_fmt,profile,level:packet=pts_time,flags:format=start_time:format_tags=title",
        ])
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
    let audio_formats: Vec<String> = crate::win::hidden_command(crate::paths::ffprobe_exe())
        .args(["-v", "error", "-select_streams", "a", "-show_entries", "stream=codec_name,sample_rate,channels", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.trim().replace(',', " "))
                .collect()
        })
        .unwrap_or_default();
    let field = |key: &str| match &stream[key] {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => String::new(),
    };
    let video_format = ["codec_name", "pix_fmt", "profile", "level"].map(field).join(" ");
    let title = json["format"]["tags"]["title"].as_str().map(str::to_string).filter(|t| !t.trim().is_empty());
    Ok(ClipProbe {
        path: path.to_path_buf(),
        width: stream["width"].as_u64().unwrap_or(0) as u32,
        height: stream["height"].as_u64().unwrap_or(0) as u32,
        fps,
        times,
        keyframes,
        bytes: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        video_offset: (t0 - file_start).max(0.0),
        audio_tracks: audio_formats.len(),
        video_format,
        audio_formats,
        title,
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
    let part = PartFiles(vec![dir.join(format!(".{stem}.trim.part.mp4"))]);
    log::info!("trim {:?}: {}..={} of {} -> {}", job.mode, job.start, job.end, src.display(), final_path.display());
    cut(probe, job.start, job.end, job.mode, job.encoder, None, &part.0[0], progress, cancel)?;
    // The trimmed clip keeps the original's dates, so it stays in its place
    // in date-sorted lists instead of jumping to "newest".
    finish(&part.0[0], &final_path, ClipTimes::of(src))?;
    progress(1.0);
    Ok(final_path)
}

/// Writes frames `start..=end` of `probe`'s clip to `out` (an mp4). With
/// `fit` (Precise joins), the result has exactly that format. Returns how
/// many frames it wrote.
#[allow(clippy::too_many_arguments)]
fn cut(
    probe: &ClipProbe,
    start: usize,
    end: usize,
    mode: TrimMode,
    encoder: EncoderKind,
    fit: Option<&Fit>,
    out: &Path,
    progress: &dyn Fn(f32),
    cancel: &AtomicBool,
) -> Result<usize> {
    let src = &probe.path;
    let start = effective_start(probe, start, mode);
    let mut frames = end - start + 1;
    let half = probe.frame_duration() / 2.0;
    let mut cmd = std::process::Command::new(crate::paths::ffmpeg_exe());
    cmd.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-progress", "pipe:1", "-nostats"]);
    match mode {
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
            cmd.args(["-ss", &format!("{:.6}", probe.seek_time(start, -half))]).arg("-i").arg(src);
            let (fps, kbps) = match fit {
                None => {
                    cmd.args(["-map", "0:v:0", "-map", "0:a?", "-c:a", "aac", "-b:a", "160k"]);
                    (probe.fps.round().max(1.0) as u32, (probe.bitrate_kbps() as f64 * 1.25).max(8000.0) as u32)
                }
                Some(f) => {
                    // Same size (fitted, not stretched), frame rate and sound
                    // tracks for every piece, so the parts can be joined.
                    let (w, h) = (f.width, f.height);
                    let seconds = frames as f64 / probe.fps.max(1.0);
                    frames = ((seconds * f.fps as f64).round() as usize).max(1);
                    if probe.audio_tracks < f.audio_tracks {
                        // Silence for the tracks this clip doesn't have
                        // (input 1, right after the clip).
                        cmd.args(["-f", "lavfi", "-t", &format!("{:.3}", seconds + 0.5), "-i", "anullsrc=r=48000:cl=stereo"]);
                    }
                    cmd.args(["-map", "0:v:0"]);
                    for track in 0..f.audio_tracks {
                        if track < probe.audio_tracks {
                            cmd.args(["-map", &format!("0:a:{track}")]);
                        } else {
                            cmd.args(["-map", "1:a"]);
                        }
                    }
                    cmd.args([
                        "-vf",
                        &format!("scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1,fps={}", f.fps),
                    ]);
                    cmd.args(["-c:a", "aac", "-b:a", "160k", "-ar", "48000", "-ac", "2"]);
                    (f.fps, f.kbps)
                }
            };
            cmd.args(crate::encode::ffmpeg::video_codec_args(encoder, fps, kbps, 1, false));
        }
    }
    cmd.args(["-frames:v", &frames.to_string(), "-movflags", "+faststart", "-f", "mp4"])
        .arg(out)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run(cmd, frames, progress, cancel)?;
    Ok(frames)
}

/// When a file's video starts, and when the file starts (its earliest
/// stream), in the file's own time.
fn video_start(path: &Path) -> Result<(f64, f64)> {
    let out = crate::win::hidden_command(crate::paths::ffprobe_exe())
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=start_time:format=start_time", "-of", "json"])
        .arg(path)
        .output()
        .context("could not run ffprobe")?;
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).context("ffprobe output")?;
    let num = |v: &serde_json::Value| v.as_str().and_then(|s| s.parse::<f64>().ok());
    let video = num(&json["streams"][0]["start_time"]).context("no video start time")?;
    Ok((video, num(&json["format"]["start_time"]).unwrap_or(video).min(video)))
}

/// How long the concat demuxer should take each part to last, and where to
/// cut its packets, so the next part's video follows exactly one frame
/// after this part's last frame. By default a part lasts as long as its
/// longest stream (often the sound, a few ms longer than the video), which
/// leaves a hitch in the video at every join. `parts`: (frames, video
/// start, file start) of each part in its own time; `frame` is 1/fps.
/// Returns (duration, outpoint) for every part but the last.
fn join_points(parts: &[(usize, f64, f64)], frame: f64) -> Vec<Option<(f64, f64)>> {
    (0..parts.len())
        .map(|i| {
            let (frames, video, file) = parts[i];
            let (_, next_video, next_file) = *parts.get(i + 1)?;
            // The next part's video starts (next_video - next_file) into it
            // (its sound can begin a little earlier).
            let duration = (video - file) + frames as f64 * frame - (next_video - next_file);
            // Packets from here on are dropped (this part's sound running
            // into the next part's), but never this part's last frame.
            let last_frame = video + (frames as f64 - 1.0) * frame;
            Some((duration, (file + duration).max(last_frame + 0.0002)))
        })
        .collect()
}

/// Runs an ffmpeg that reports `-progress` on stdout; `frames` is how many
/// it will write (for the progress fraction).
fn run(mut cmd: std::process::Command, frames: usize, progress: &dyn Fn(f32), cancel: &AtomicBool) -> Result<()> {
    let mut child = cmd.spawn().context("could not run ffmpeg")?;
    let stdout = child.stdout.take().context("no stdout")?;
    // stderr is read on its own thread: if ffmpeg wrote a lot of errors
    // while we only read stdout, both would wait on each other.
    let mut stderr = child.stderr.take().context("no stderr")?;
    let errors = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut s);
        s
    });
    for line in BufReader::new(stdout).lines().map_while(|l| l.ok()) {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("cancelled");
        }
        if let Some(f) = line.strip_prefix("frame=").and_then(|v| v.trim().parse::<f32>().ok()) {
            progress((f / frames.max(1) as f32).min(1.0));
        }
    }
    let status = child.wait()?;
    let errors = errors.join().unwrap_or_default();
    if !status.success() {
        bail!("ffmpeg failed: {}", errors.trim());
    }
    Ok(())
}

/// Moves the finished `part` to `final_path` and stamps `times` on it.
/// Explorer or a video player can briefly hold the original open, so this
/// retries for a couple of seconds.
fn finish(part: &Path, final_path: &Path, times: Option<ClipTimes>) -> Result<()> {
    let mut last_err = None;
    for _ in 0..20 {
        match std::fs::rename(part, final_path) {
            Ok(()) => {
                if let Some(t) = &times {
                    if let Err(e) = t.apply(final_path) {
                        log::warn!("couldn't keep the clip's date on {}: {e}", final_path.display());
                    }
                }
                return Ok(());
            }
            Err(e) => last_err = Some(e),
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(last_err.unwrap()).with_context(|| format!("could not write {} (is it open in another program?)", final_path.display()))
}

/// Temporary files, deleted when dropped (finished, failed or cancelled).
struct PartFiles(Vec<PathBuf>);

impl Drop for PartFiles {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// The format every piece is fitted to in a Precise join.
struct Fit {
    width: u32,
    height: u32,
    fps: u32,
    kbps: u32,
    audio_tracks: usize,
}

/// One piece of an edit: frames `start..=end` (inclusive) of a clip.
#[derive(Clone, Debug)]
pub struct Piece {
    pub probe: Arc<ClipProbe>,
    pub start: usize,
    pub end: usize,
}

impl Piece {
    /// The whole clip.
    pub fn whole(probe: Arc<ClipProbe>) -> Piece {
        let end = probe.frame_count().saturating_sub(1);
        Piece { probe, start: 0, end }
    }

    /// First frame that will be in the output.
    pub fn first(&self, mode: TrimMode) -> usize {
        effective_start(&self.probe, self.start, mode)
    }

    pub fn frames(&self, mode: TrimMode) -> usize {
        self.end + 1 - self.first(mode).min(self.end)
    }

    pub fn seconds(&self, mode: TrimMode) -> f64 {
        self.frames(mode) as f64 * self.probe.frame_duration()
    }

    /// Both halves of a split at `frame`, which starts the second half.
    /// None unless `frame` is inside the piece (after its first frame).
    pub fn split_at(&self, frame: usize) -> Option<(Piece, Piece)> {
        if frame <= self.start || frame > self.end {
            return None;
        }
        let a = Piece { probe: self.probe.clone(), start: self.start, end: frame - 1 };
        let b = Piece { probe: self.probe.clone(), start: frame, end: self.end };
        Some((a, b))
    }

    pub fn same_clip(&self, other: &Piece) -> bool {
        Arc::ptr_eq(&self.probe, &other.probe) || self.probe.path == other.probe.path
    }
}

/// Why `pieces` can't be joined without re-encoding (None: they can).
pub fn fast_join_problem(pieces: &[Piece]) -> Option<&'static str> {
    let first = &pieces.first()?.probe;
    for p in pieces.iter().map(|p| &p.probe) {
        if (p.width, p.height) != (first.width, first.height) {
            return Some("The clips have different sizes");
        }
        if (p.fps - first.fps).abs() > 0.01 {
            return Some("The clips have different frame rates");
        }
        if p.video_format != first.video_format {
            return Some("The clips' video formats differ");
        }
        if p.audio_formats != first.audio_formats {
            return Some("The clips' sound tracks differ");
        }
    }
    None
}

#[derive(Clone, Debug)]
pub struct EditJob {
    pub pieces: Vec<Piece>,
    pub mode: TrimMode,
    /// Each piece as its own file (`_trim` names) instead of one joined clip.
    pub separate: bool,
    /// Encoder for precise mode.
    pub encoder: EncoderKind,
}

/// Exports an edit: one new file per piece (`separate`), else one clip with
/// every piece in order (`<first clip>_edit.mp4`, or `_trim.mp4` for a
/// single piece). `progress` gets 0..=1. Set `cancel` to abort.
pub fn export(job: &EditJob, progress: &dyn Fn(f32), cancel: &AtomicBool) -> Result<Vec<PathBuf>> {
    let pieces = &job.pieces;
    for p in pieces {
        if p.start > p.end || p.end >= p.probe.frame_count() {
            bail!("invalid piece {}..={} of {}", p.start, p.end, p.probe.path.display());
        }
    }
    let total: usize = pieces.iter().map(|p| p.frames(job.mode)).sum();
    let share = |done: usize, frames: usize, f: f32| (done as f32 + f * frames as f32) / total.max(1) as f32;
    if job.separate || pieces.len() == 1 {
        let mut out = Vec::new();
        let mut done = 0;
        for p in pieces {
            if cancel.load(Ordering::Relaxed) {
                bail!("cancelled");
            }
            let frames = p.frames(job.mode);
            let trim_job = TrimJob { start: p.start, end: p.end, mode: job.mode, output: Output::NewFile, encoder: job.encoder };
            out.push(trim(&p.probe, &trim_job, &|f| progress(share(done, frames, f)), cancel)?);
            done += frames;
        }
        return Ok(out);
    }
    join(job, total, &|done, frames, f| progress(share(done, frames, f) * 0.97), cancel).map(|p| {
        progress(1.0);
        vec![p]
    })
}

/// Pieces in order into one clip, next to the first piece's clip.
fn join(job: &EditJob, total: usize, progress: &dyn Fn(usize, usize, f32), cancel: &AtomicBool) -> Result<PathBuf> {
    let first = &job.pieces[0].probe;
    if job.mode == TrimMode::Fast {
        if let Some(why) = fast_join_problem(&job.pieces) {
            bail!("{why}: they can only be joined in Precise mode");
        }
    }
    let dir = first.path.parent().unwrap_or(Path::new("."));
    let stem = first.path.file_stem().unwrap_or_default().to_string_lossy().to_string();
    let final_path = crate::win::unique_path(dir, &format!("{stem}_edit"), "mp4");
    let fit = (job.mode == TrimMode::Precise).then(|| Fit {
        // Even sizes (the encoders need them).
        width: first.width.max(2) & !1,
        height: first.height.max(2) & !1,
        fps: first.fps.round().max(1.0) as u32,
        kbps: job.pieces.iter().map(|p| (p.probe.bitrate_kbps() as f64 * 1.25) as u32).max().unwrap_or(0).max(8000),
        audio_tracks: job.pieces.iter().map(|p| p.probe.audio_tracks).max().unwrap_or(0),
    });
    log::info!(
        "join {:?}: {} pieces, {total} frames -> {}",
        job.mode,
        job.pieces.len(),
        final_path.display()
    );
    let mut parts = PartFiles(Vec::new());
    let mut timing = Vec::new();
    let mut done = 0;
    for (i, p) in job.pieces.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let part = dir.join(format!(".{stem}.edit.{i}.part.mp4"));
        parts.0.push(part.clone());
        let frames = p.frames(job.mode);
        let written = cut(&p.probe, p.start, p.end, job.mode, job.encoder, fit.as_ref(), &part, &|f| progress(done, frames, f), cancel)?;
        let (video, file) = video_start(&part)?;
        timing.push((written, video, file));
        done += frames;
    }

    // The concat demuxer reads a list of files (relative to the list).
    let list = dir.join(format!(".{stem}.edit.txt"));
    let out = dir.join(format!(".{stem}.edit.part.mp4"));
    let fps = fit.as_ref().map_or(first.fps, |f| f.fps as f64);
    let points = join_points(&timing, 1.0 / fps.max(1.0));
    let mut text = String::from("ffconcat version 1.0\n");
    for (p, point) in parts.0.iter().zip(&points) {
        let name = p.file_name().unwrap_or_default().to_string_lossy();
        text.push_str(&format!("file '{}'\n", name.replace('\'', r"'\''")));
        if let Some((duration, outpoint)) = point {
            text.push_str(&format!("duration {duration:.6}\noutpoint {outpoint:.6}\n"));
        }
    }
    parts.0.push(list.clone());
    parts.0.push(out.clone());
    std::fs::write(&list, text).context("could not write the join list")?;
    let mut cmd = std::process::Command::new(crate::paths::ffmpeg_exe());
    cmd.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-progress", "pipe:1", "-nostats"])
        .args(["-f", "concat", "-safe", "0", "-i"])
        .arg(&list)
        .args(["-map", "0", "-c", "copy"]);
    if let Some(t) = &first.title {
        cmd.args(["-metadata", &format!("title={t}")]);
    }
    cmd.args(["-movflags", "+faststart", "-f", "mp4"]).arg(&out).stdout(Stdio::piped()).stderr(Stdio::piped());
    run(cmd, total, &|_| {}, cancel)?;
    // Dated like the first clip, as a trim is (the name carries its date too).
    finish(&out, &final_path, ClipTimes::of(&first.path))?;
    Ok(final_path)
}

/// A file's created / modified / accessed times.
pub struct ClipTimes {
    created: std::time::SystemTime,
    modified: std::time::SystemTime,
    accessed: std::time::SystemTime,
}

impl ClipTimes {
    pub fn of(path: &Path) -> Option<ClipTimes> {
        let m = std::fs::metadata(path).ok()?;
        let modified = m.modified().ok()?;
        Some(ClipTimes {
            created: m.created().unwrap_or(modified),
            modified,
            accessed: m.accessed().unwrap_or(modified),
        })
    }

    /// Stamps these times onto `path`.
    pub fn apply(&self, path: &Path) -> std::io::Result<()> {
        use std::os::windows::fs::FileTimesExt;
        let times = std::fs::FileTimes::new()
            .set_modified(self.modified)
            .set_accessed(self.accessed)
            .set_created(self.created);
        std::fs::OpenOptions::new().write(true).open(path)?.set_times(times)
    }
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
            video_format: "h264 yuv420p High 42".into(),
            audio_formats: vec!["aac 48000 2".into()],
            title: None,
        }
    }

    #[test]
    fn joins_line_each_part_up_after_the_last_frame() {
        let f = 1.0 / 60.0;
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        // Video first in every part: each lasts exactly its frames.
        let p = join_points(&[(60, 0.0, 0.0), (30, 0.0, 0.0), (90, 0.0, 0.0)], f);
        assert_eq!(p.len(), 3);
        assert!(close(p[0].unwrap().0, 1.0) && close(p[0].unwrap().1, 1.0));
        assert!(close(p[1].unwrap().0, 0.5));
        assert_eq!(p[2], None, "the last part ends on its own");
        // The next part's sound starts 16 ms before its video (as in a
        // real fast cut): this part ends 16 ms early, after its last frame.
        let p = join_points(&[(60, 0.000065, 0.0), (60, 0.016081, 0.0)], f);
        let (duration, outpoint) = p[0].unwrap();
        assert!(close(duration, 0.000065 + 1.0 - 0.016081));
        assert!(close(outpoint, duration));
        // Video starting late in this part counts too.
        let p = join_points(&[(60, 0.010, 0.0), (60, 0.0, 0.0)], f);
        assert!(close(p[0].unwrap().0, 1.010));
        // Sound starting more than a frame early: the next part still lines
        // up (duration), but this part's last frame is never cut.
        let p = join_points(&[(60, 0.0, 0.0), (60, 0.020, 0.0)], f);
        let (duration, outpoint) = p[0].unwrap();
        assert!(close(duration, 0.98));
        assert!(outpoint > 59.0 / 60.0);
    }

    #[test]
    fn pieces_split_and_join_checks() {
        let clip = Arc::new(probe_with(vec![0, 60, 120], 180));
        let whole = Piece::whole(clip.clone());
        assert_eq!((whole.start, whole.end), (0, 179));
        // Split: the frame starts the second half; never an empty half.
        let (a, b) = whole.split_at(75).unwrap();
        assert_eq!((a.start, a.end, b.start, b.end), (0, 74, 75, 179));
        assert!(whole.split_at(0).is_none() && whole.split_at(180).is_none());
        assert!(a.split_at(75).is_none(), "outside the piece");
        // Fast mode: the second half starts on the keyframe before 75.
        assert_eq!(b.first(TrimMode::Fast), 60);
        assert_eq!(b.frames(TrimMode::Fast), 120);
        assert_eq!(b.frames(TrimMode::Precise), 105);
        assert!((b.seconds(TrimMode::Precise) - 1.75).abs() < 1e-9);
        assert!(a.same_clip(&b));

        // Pieces of one clip always join losslessly; other clips only when
        // their size, frame rate and formats match.
        assert_eq!(fast_join_problem(&[a.clone(), b.clone()]), None);
        let mut other = probe_with(vec![0, 60], 120);
        other.path = PathBuf::from("other.mp4");
        assert_eq!(fast_join_problem(&[a.clone(), Piece::whole(Arc::new(other.clone()))]), None);
        other.width = 1280;
        assert!(fast_join_problem(&[a.clone(), Piece::whole(Arc::new(other.clone()))]).is_some());
        other.width = 1920;
        other.audio_formats.clear();
        assert!(fast_join_problem(&[a.clone(), Piece::whole(Arc::new(other.clone()))]).is_some());
        other.audio_formats = vec!["aac 48000 2".into()];
        other.fps = 30.0;
        assert!(fast_join_problem(&[a, Piece::whole(Arc::new(other))]).is_some());
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
