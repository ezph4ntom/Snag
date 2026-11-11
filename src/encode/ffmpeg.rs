//! The ffmpeg sidecar encoder.
//!
//! We start `ffmpeg.exe` with raw NV12 frames on stdin and let its HLS muxer
//! write fragmented-MP4 segments. Why HLS/fMP4?
//! * Crash-safe: every finished segment is a self-contained, valid file.
//!   If the app or ffmpeg dies, everything up to the last second survives.
//! * Rolling buffer for free: `delete_segments` removes old segments, and
//!   `temp_file` means a segment only gets its final name once complete.
//! * The playlist tells us exactly which segments are complete.

use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use super::{AudioSink, EncoderConfig, EncoderKind, Nv12Frame, VideoEncoder};
use crate::win::{ABOVE_NORMAL_PRIORITY_CLASS, CREATE_NO_WINDOW};

/// File names inside a session folder (see the contract in `encode/mod.rs`).
pub const PLAYLIST: &str = "buffer.m3u8";
pub const INIT_SEGMENT: &str = "init.mp4";

/// Frames written by `prime()`. Our pipe holds two frames, so the third
/// write can only complete once ffmpeg's process has started and is reading
/// (measured: ~0.2 s after spawn). ffmpeg drops these frames (`trim`).
const PRIME_FRAMES: usize = 3;

/// Timeline audio written as silence up front (0.5 s), see `FfmpegAudioSink::open`.
const AUDIO_PREROLL_FRAMES: usize = crate::audio::SAMPLE_RATE as usize / 2;

/// Tags the raw frames as BT.709 limited range (what our GPU converter
/// produces). This must be a *filter* that only sets metadata: asking for
/// `-colorspace bt709 -color_range tv` on the output instead makes ffmpeg
/// insert a hidden software color conversion of every frame (measured: ~3
/// CPU cores at 1080p60), because raw input has "unknown" color properties.
const COLOR_TAGS: &str = "setparams=range=tv:colorspace=bt709:color_primaries=bt709:color_trc=bt709";

/// DXGI adapter index of the GPU that owns `kind`'s encoder (NVIDIA for
/// NVENC, AMD for AMF), for uploading frames as D3D11 textures.
pub fn encoder_adapter(kind: EncoderKind) -> Option<u32> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
    let vendor = match kind {
        EncoderKind::Nvenc => 0x10DE,
        EncoderKind::Amf => 0x1002,
        _ => return None,
    };
    // SAFETY: plain DXGI enumeration; EnumAdapters1 errors past the end.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        (0..16u32).find(|&i| {
            factory
                .EnumAdapters1(i)
                .and_then(|a| a.GetDesc1())
                .is_ok_and(|d| d.VendorId == vendor)
        })
    }
}

/// How frames get from our pipe into the encoder.
///
/// With `gpu_upload`, ffmpeg uploads each frame into a D3D11 texture on the
/// encoder's GPU, so NVENC/AMF run on D3D11. Without it, NVENC creates a
/// CUDA context, which doubles ffmpeg's memory (measured peak: ~200 MB vs
/// ~410 MB). Returns `(args before -i, -vf filter chain)`.
pub fn input_path_args(kind: EncoderKind, gpu_upload: bool) -> (Vec<String>, String) {
    match encoder_adapter(kind).filter(|_| gpu_upload) {
        Some(adapter) => (
            vec![
                "-init_hw_device".into(),
                format!("d3d11va=gpu:{adapter}"),
                "-filter_hw_device".into(),
                "gpu".into(),
            ],
            format!("{COLOR_TAGS},hwupload"),
        ),
        None => (Vec::new(), COLOR_TAGS.to_string()),
    }
}

/// Codec arguments for `kind`, shared by live recording and precise trims.
///
/// Every encoder gets:
/// * an IDR keyframe exactly every `keyframe_seconds` (forced at exact
///   timestamps, scene-cut detection off), so every segment, and therefore
///   every clip, starts on a clean, independently decodable frame;
/// * no B-frames: no frame reordering, so packet order = display order and
///   there are no composition-time offsets to get wrong when concatenating;
/// * VBR with a peak cap, so static scenes don't waste disk.
///
/// `live`: tuned for recording while gaming (x264 skips its lookahead,
/// which cuts its CPU use ~5x and memory ~2x for slightly lower quality).
pub fn video_codec_args(kind: EncoderKind, fps: u32, bitrate_kbps: u32, keyframe_seconds: u32, live: bool) -> Vec<String> {
    let gop = (fps * keyframe_seconds).max(1);
    let b = bitrate_kbps.max(500);
    let mut a: Vec<String> = match kind {
        EncoderKind::Nvenc => vec![
            "-c:v", "h264_nvenc", "-preset", "p4", "-tune", "hq", "-rc", "vbr",
            "-forced-idr", "1", "-no-scenecut", "1", "-profile:v", "high",
        ],
        EncoderKind::Amf => vec![
            "-c:v", "h264_amf", "-usage", "transcoding", "-quality", "balanced", "-rc", "vbr_peak",
            "-forced_idr", "1", "-profile:v", "high",
        ],
        EncoderKind::Qsv => vec![
            "-c:v", "h264_qsv", "-preset", "faster", "-look_ahead", "0", "-forced_idr", "1",
            "-idr_interval", "0", "-profile:v", "high",
        ],
        EncoderKind::X264 => vec!["-c:v", "libx264", "-preset", "veryfast", "-profile:v", "high"],
    }
    .into_iter()
    .map(String::from)
    .collect();

    if kind == EncoderKind::X264 {
        // Fixed GOP, no scene-cut keyframes. Leave the game at least half the
        // CPU threads: x264 would otherwise grab 1.5x the core count.
        a.push("-x264-params".into());
        a.push(format!("keyint={gop}:min-keyint={gop}:scenecut=0"));
        let threads = (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) / 2).max(2);
        a.push("-threads".into());
        a.push(threads.to_string());
        if live {
            a.push("-tune".into());
            a.push("zerolatency".into());
        }
    }
    a.extend(
        [
            "-b:v".to_string(),
            format!("{b}k"),
            "-maxrate".into(),
            format!("{}k", b * 3 / 2),
            "-bufsize".into(),
            format!("{}k", b * 2),
            "-g".into(),
            gop.to_string(),
            "-bf".into(),
            "0".into(),
            // Keyframe at t = 0, k, 2k, ... seconds exactly.
            "-force_key_frames".into(),
            format!("expr:gte(t,n_forced*{keyframe_seconds})"),
        ],
    );
    a
}

/// A running ffmpeg process fed through its stdin.
pub struct FfmpegHlsEncoder {
    child: Child,
    stdin: Option<std::fs::File>,
    stderr_thread: Option<JoinHandle<()>>,
    recent_errors: Arc<Mutex<Vec<String>>>,
    frame_bytes: usize,
    /// Handed out once via `take_audio_sink`.
    audio_sink: Option<FfmpegAudioSink>,
    /// Name + "ffmpeg connected" flag of the audio pipe, so `finish` can
    /// unblock an audio thread still waiting for ffmpeg to connect.
    audio_pipe: Option<(String, Arc<AtomicBool>)>,
}

/// Creates the server end of a named pipe that ffmpeg opens as its audio
/// input. stdin is already used for video, and a named pipe is the simplest
/// second channel into a child process on Windows.
fn create_audio_pipe() -> Result<(String, std::fs::File)> {
    use windows::Win32::Storage::FileSystem::PIPE_ACCESS_OUTBOUND;
    use windows::Win32::System::Pipes::{CreateNamedPipeW, PIPE_TYPE_BYTE, PIPE_WAIT};
    use windows::core::PCWSTR;
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let name = format!(
        r"\\.\pipe\snag-audio-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let wide = crate::win::to_wide(&name);
    // SAFETY: valid NUL-terminated name; we own the returned handle.
    let handle = unsafe {
        CreateNamedPipeW(PCWSTR(wide.as_ptr()), PIPE_ACCESS_OUTBOUND, PIPE_TYPE_BYTE | PIPE_WAIT, 1, 1 << 20, 0, 0, None)
    };
    if handle.is_invalid() {
        bail!("could not create the audio pipe: {}", std::io::Error::last_os_error());
    }
    // SAFETY: a valid handle we exclusively own from here on.
    Ok((name, unsafe { std::fs::File::from_raw_handle(handle.0 as _) }))
}

/// A WAV header for an endless stream of 48 kHz stereo float samples.
///
/// Why WAV and not ffmpeg's raw `f32le` input? The raw PCM reader hands
/// audio over in ~85 ms packets (it aims for 10 packets per second), and
/// ffmpeg pauses reading an input that gets more than ~100 ms ahead of the
/// others. With audio arriving in 85 ms lumps, the *video* input kept being
/// paused, our frame pipe filled up and the pacer dropped frames (measured).
/// The WAV reader uses small packets (`-max_size 4096` = ~11 ms).
fn streaming_wav_header() -> Vec<u8> {
    let rate = crate::audio::SAMPLE_RATE;
    let channels = crate::audio::CHANNELS as u16;
    let block = channels * 4;
    let mut h = Vec::with_capacity(44);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&u32::MAX.to_le_bytes()); // unknown length (stream)
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&3u16.to_le_bytes()); // WAVE_FORMAT_IEEE_FLOAT
    h.extend_from_slice(&channels.to_le_bytes());
    h.extend_from_slice(&rate.to_le_bytes());
    h.extend_from_slice(&(rate * block as u32).to_le_bytes());
    h.extend_from_slice(&block.to_le_bytes());
    h.extend_from_slice(&32u16.to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&u32::MAX.to_le_bytes());
    h
}

/// Writes PCM into ffmpeg's audio input pipe.
pub struct FfmpegAudioSink {
    pipe: std::fs::File,
    connected: Arc<AtomicBool>,
    /// Silence ffmpeg trims off the front (matches the priming video frames).
    prime_frames: usize,
}

impl AudioSink for FfmpegAudioSink {
    fn open(&mut self) -> Result<u64> {
        use windows::Win32::Foundation::{ERROR_PIPE_CONNECTED, HANDLE};
        use windows::Win32::System::Pipes::ConnectNamedPipe;
        // SAFETY: our own pipe handle; blocks until ffmpeg opens the pipe.
        let r = unsafe { ConnectNamedPipe(HANDLE(self.pipe.as_raw_handle() as _), None) };
        if let Err(e) = r {
            if e.code() != ERROR_PIPE_CONNECTED.to_hresult() {
                bail!("ffmpeg didn't open the audio pipe: {e}");
            }
        }
        self.connected.store(true, Ordering::Relaxed);
        self.pipe.write_all(&streaming_wav_header()).context("ffmpeg stopped accepting audio")?;
        // ffmpeg reads audio while it starts up, before the stream clock runs:
        // the priming part (trimmed away, matching the priming video frames)
        // plus half a second of the timeline itself. Until ffmpeg has all
        // its inputs open it reads no video at all, so it must never have to
        // wait for audio then. (Costs the first 0.5 s of each session's audio.)
        let frames = self.prime_frames + AUDIO_PREROLL_FRAMES;
        self.write(&vec![0.0; frames * crate::audio::CHANNELS])?;
        Ok(AUDIO_PREROLL_FRAMES as u64)
    }

    fn write(&mut self, samples: &[f32]) -> Result<()> {
        // SAFETY: viewing f32s as bytes; x86 is little-endian, matching
        // ffmpeg's `f32le` input format.
        let bytes = unsafe { std::slice::from_raw_parts(samples.as_ptr() as *const u8, std::mem::size_of_val(samples)) };
        self.pipe.write_all(bytes).context("ffmpeg stopped accepting audio")
    }
}

impl FfmpegHlsEncoder {
    pub fn spawn(cfg: &EncoderConfig) -> Result<Self> {
        std::fs::create_dir_all(&cfg.segment_dir)
            .with_context(|| format!("creating {}", cfg.segment_dir.display()))?;
        let frame_bytes = (cfg.width * cfg.height * 3 / 2) as usize;
        // Pipe big enough for ~2 frames so a write rarely has to wait.
        let (pipe_read, pipe_write) = crate::win::big_pipe((frame_bytes * 2).min(64 << 20) as u32)?;

        // Optional second input: PC audio through a named pipe.
        let audio = if cfg.audio { Some(create_audio_pipe()?) } else { None };
        let prime_audio = (PRIME_FRAMES as u64 * crate::audio::SAMPLE_RATE as u64 / cfg.fps.max(1) as u64) as usize;

        let ffmpeg = crate::paths::ffmpeg_exe();
        let mut cmd = std::process::Command::new(ffmpeg);
        // Hardware encoders barely use the CPU, so above-normal priority costs
        // the game nothing and protects us from being starved. x264 does real
        // CPU work, so it runs at normal priority to not slow the game.
        let priority = if cfg.kind.is_hardware() { ABOVE_NORMAL_PRIORITY_CLASS } else { 0 };
        let (hw_args, filters) = input_path_args(cfg.kind, cfg.gpu_upload);
        // Drop the priming frames (and the matching audio) and restart
        // timestamps at 0, so frame 0 and audio sample 0 are the same instant.
        let mut graph = format!("[0:v]trim=start_frame={PRIME_FRAMES},setpts=PTS-STARTPTS,{filters}[v]");
        if audio.is_some() {
            graph.push_str(&format!(";[1:a]atrim=start_sample={prime_audio},asetpts=PTS-STARTPTS[a]"));
        }
        cmd.creation_flags(CREATE_NO_WINDOW | priority)
            .current_dir(&cfg.segment_dir)
            .args(["-hide_banner", "-loglevel", "warning", "-nostats"])
            .args(&hw_args)
            .args(["-f", "rawvideo", "-pix_fmt", "nv12"])
            .args(["-video_size", &format!("{}x{}", cfg.width, cfg.height)])
            .args(["-framerate", &cfg.fps.to_string()])
            // ffmpeg stops pulling frames while the encoder initializes
            // (~0.3 s for NVENC). A deeper input queue lets its reader thread
            // keep draining our pipe meanwhile, so startup never stalls us.
            .args(["-thread_queue_size", "48"])
            // Formats are fully specified: don't wait to "probe" the inputs.
            .args(["-probesize", "32", "-analyzeduration", "0"])
            .args(["-i", "pipe:0"]);
        if let Some((name, _)) = &audio {
            cmd.args(["-f", "wav", "-ignore_length", "1", "-max_size", "4096"])
                .args(["-probesize", "32", "-analyzeduration", "0"])
                .args(["-thread_queue_size", "512", "-i", name]);
        }
        cmd.args(["-filter_complex", &graph, "-map", "[v]"])
            .args(video_codec_args(cfg.kind, cfg.fps, cfg.bitrate_kbps, cfg.segment_seconds, true));
        if audio.is_some() {
            cmd.args(["-map", "[a]", "-c:a", "aac", "-b:a", &format!("{}k", cfg.audio_bitrate_kbps.max(64))]);
        }
        cmd.args(["-f", "hls", "-hls_time", &cfg.segment_seconds.to_string()])
            .args(["-hls_segment_type", "fmp4", "-hls_fmp4_init_filename", INIT_SEGMENT])
            .args(["-hls_list_size", &cfg.keep_segments.to_string()])
            // Keep a few segments past the playlist so a clip being saved
            // can't lose its oldest segment to deletion mid-copy.
            .args(["-hls_delete_threshold", "4"])
            .args(["-hls_flags", "delete_segments+independent_segments+temp_file"])
            .args(["-hls_segment_filename", "seg_%06d.m4s", "-start_number", "0"])
            .arg(PLAYLIST)
            .stdin(Stdio::from(pipe_read))
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        log::info!(
            "starting encoder: {}{}{} {}x{} @ {} fps, {} kbps, segments of {} s in {}",
            cfg.kind.ffmpeg_codec(),
            if hw_args.is_empty() { "" } else { " (D3D11 upload)" },
            if audio.is_some() { " + PC audio (AAC)" } else { "" },
            cfg.width,
            cfg.height,
            cfg.fps,
            cfg.bitrate_kbps,
            cfg.segment_seconds,
            cfg.segment_dir.display()
        );
        let mut child = cmd
            .spawn()
            .with_context(|| format!("could not start {} (see README: ffmpeg setup)", ffmpeg.display()))?;
        // `cmd` still owns the child's copy of the read end; drop it so the
        // child sees EOF once we close our write end.
        drop(cmd);

        let recent_errors = Arc::new(Mutex::new(Vec::new()));
        let stderr = child.stderr.take().context("no stderr")?;
        let errs = recent_errors.clone();
        let stderr_thread = std::thread::Builder::new()
            .name("ffmpeg-stderr".into())
            .spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(|l| l.ok()) {
                    let line = line.trim().to_string();
                    if line.is_empty() {
                        continue;
                    }
                    // Expected notices about our endless WAV audio stream: not problems.
                    // The last two appear when the audio pipe closes mid-packet at stop.
                    const BENIGN: [&str; 4] =
                        ["Ignoring maximum wav data size", "Guessed Channel Layout", "Packet corrupt", "corrupt input packet"];
                    if BENIGN.iter().any(|b| line.contains(b)) {
                        log::debug!("ffmpeg: {line}");
                        continue;
                    }
                    log::warn!("ffmpeg: {line}");
                    if let Ok(mut e) = errs.lock() {
                        e.push(line);
                        let excess = e.len().saturating_sub(5);
                        e.drain(..excess);
                    }
                }
            })?;

        let connected = Arc::new(AtomicBool::new(false));
        let (audio_sink, audio_pipe) = match audio {
            Some((name, pipe)) => (
                Some(FfmpegAudioSink { pipe, connected: connected.clone(), prime_frames: prime_audio }),
                Some((name, connected)),
            ),
            None => (None, None),
        };
        Ok(Self {
            child,
            stdin: Some(pipe_write),
            stderr_thread: Some(stderr_thread),
            recent_errors,
            frame_bytes,
            audio_sink,
            audio_pipe,
        })
    }

    fn error_context(&self) -> String {
        self.recent_errors
            .lock()
            .map(|e| e.join(" | "))
            .unwrap_or_default()
    }
}

impl VideoEncoder for FfmpegHlsEncoder {
    fn prime(&mut self, frame: &Nv12Frame) -> Result<()> {
        let started = std::time::Instant::now();
        for _ in 0..PRIME_FRAMES {
            self.encode(frame)?;
        }
        log::info!("encoder ready after {} ms", started.elapsed().as_millis());
        Ok(())
    }

    fn encode(&mut self, frame: &Nv12Frame) -> Result<()> {
        let stdin = self.stdin.as_mut().ok_or_else(|| anyhow!("encoder already finished"))?;
        debug_assert_eq!(frame.data.len(), self.frame_bytes);
        stdin.write_all(&frame.data).map_err(|e| {
            // Give ffmpeg a moment to print why it died.
            std::thread::sleep(Duration::from_millis(100));
            anyhow!("ffmpeg stopped accepting frames ({e}): {}", self.error_context())
        })
    }

    fn take_audio_sink(&mut self) -> Option<Box<dyn AudioSink>> {
        self.audio_sink.take().map(|s| Box::new(s) as Box<dyn AudioSink>)
    }

    fn finish(&mut self) -> Result<()> {
        // Closing stdin = end of video; ffmpeg finalizes the last segment once
        // the audio pipe closes too (the recorder stops its audio thread).
        drop(self.stdin.take());
        if let Some((name, connected)) = &self.audio_pipe {
            if !connected.load(Ordering::Relaxed) {
                // ffmpeg never connected (it failed to start): connect to the
                // pipe ourselves so the audio thread's wait returns.
                let _ = std::fs::OpenOptions::new().read(true).open(name);
            }
        }
        let status = crate::win::wait_timeout(&mut self.child, Duration::from_secs(10));
        if let Some(t) = self.stderr_thread.take() {
            let _ = t.join();
        }
        match status {
            Some(s) if s.success() => Ok(()),
            Some(s) => Err(anyhow!("ffmpeg exited with {s}: {}", self.error_context())),
            None => Err(anyhow!("ffmpeg did not exit within 10 s and was killed")),
        }
    }
}

impl Drop for FfmpegHlsEncoder {
    fn drop(&mut self) {
        if self.stdin.is_some() {
            let _ = self.finish();
        }
    }
}
