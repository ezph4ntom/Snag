//! Sound for the trimmer's preview.
//!
//! While the preview plays, an ffmpeg process decodes the clip's audio from
//! the play position into 48 kHz stereo float samples, and this thread feeds
//! them to the default playback device with WASAPI.
//!
//! Sync: the video preview runs on the wall clock ("frame = start frame +
//! elapsed time x fps"). The audio follows the same clock: when the first
//! samples are ready, it skips whatever the video has already shown plus the
//! device's output latency, so the sound you hear lines up with the frame
//! on screen. Over a clip's length the two clocks drift by well under a
//! millisecond.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    IAudioClient, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX, eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};

const RATE: u32 = 48_000;
const CHANNELS: usize = 2;
/// WAVE_FORMAT_IEEE_FLOAT (from mmreg.h).
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

pub struct AudioPlayer {
    /// The wall-clock instant at which the clip is at the start position.
    pub anchor: Instant,
    stop: Arc<AtomicBool>,
    /// Volume 0..=1 (changes apply immediately), stored as f32 bits.
    volume: Arc<AtomicU32>,
    /// Frames handed to the device so far (for tests/diagnostics).
    pub frames_played: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl AudioPlayer {
    /// Plays `path`'s first audio track from file time `from` (seconds) so
    /// that the clip is at `from` at the instant `anchor`.
    pub fn start(path: &Path, from: f64, anchor: Instant, volume: f32) -> AudioPlayer {
        let stop = Arc::new(AtomicBool::new(false));
        let vol = Arc::new(AtomicU32::new(volume.clamp(0.0, 1.0).to_bits()));
        let played = Arc::new(AtomicU64::new(0));
        let (path, s, v, p) = (path.to_path_buf(), stop.clone(), vol.clone(), played.clone());
        let thread = std::thread::Builder::new()
            .name("preview-audio".into())
            .spawn(move || {
                if let Err(e) = play(&path, from, anchor, &s, &v, &p) {
                    if !s.load(Ordering::Relaxed) {
                        log::warn!("preview audio: {e:#}");
                    }
                }
            })
            .ok();
        AudioPlayer { anchor, stop, volume: vol, frames_played: played, thread }
    }

    pub fn set_volume(&self, volume: f32) {
        self.volume.store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
}

impl AudioPlayer {
    /// Stops and waits until the thread has finished (tests).
    pub fn stop_and_wait(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        // Not joined: the thread may be waiting a moment for ffmpeg to start,
        // and pausing must not freeze the window for that. It sees the flag
        // within 5 ms of having data and exits on its own.
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Starts ffmpeg decoding the first audio track from `from` seconds into raw
/// 48 kHz stereo f32 on stdout.
pub fn spawn_decoder(path: &Path, from: f64) -> Result<Child> {
    crate::win::hidden_command(crate::paths::ffmpeg_exe())
        .args(["-hide_banner", "-loglevel", "error", "-nostdin"])
        .args(["-ss", &format!("{:.6}", from.max(0.0))])
        .arg("-i")
        .arg(path)
        .args(["-map", "0:a:0", "-vn", "-sn", "-f", "f32le", "-ac", "2", "-ar", "48000", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("could not start ffmpeg for preview audio")
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Reads up to `frames` stereo frames; returns how many were read (0 = end).
fn read_frames(src: &mut impl Read, out: &mut Vec<f32>, frames: usize) -> usize {
    let mut bytes = vec![0u8; frames * CHANNELS * 4];
    let mut got = 0;
    while got < bytes.len() {
        match src.read(&mut bytes[got..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => got += n,
        }
    }
    let whole = got / (CHANNELS * 4);
    out.clear();
    out.extend(bytes[..whole * CHANNELS * 4].as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
    whole
}

fn play(
    path: &Path,
    from: f64,
    anchor: Instant,
    stop: &AtomicBool,
    volume: &AtomicU32,
    played: &AtomicU64,
) -> Result<()> {
    let mut child = KillOnDrop(spawn_decoder(path, from)?);
    let mut pcm = child.0.stdout.take().context("no ffmpeg stdout")?;

    // SAFETY: COM init for this thread; standard WASAPI render setup.
    let (client, render, buffer_frames) = unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).context("no playback device")?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let block = (CHANNELS * 4) as u16;
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            nChannels: CHANNELS as u16,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * block as u32,
            nBlockAlign: block,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            1_000_000, // 100 ms buffer
            0,
            &format,
            None,
        )?;
        let render: IAudioRenderClient = client.GetService()?;
        let frames = client.GetBufferSize()? as usize;
        (client, render, frames)
    };

    // Wait for the first decoded samples (ffmpeg needs ~50-150 ms to start).
    let mut chunk = Vec::new();
    let first = read_frames(&mut pcm, &mut chunk, 960);
    if first == 0 || stop.load(Ordering::Relaxed) {
        return Ok(()); // no audio track, or already stopped
    }
    // SAFETY: valid client.
    let latency = Duration::from_nanos(unsafe { client.GetStreamLatency() }.unwrap_or(0) as u64 * 100);
    // The video is already `elapsed` past the start; the first sample written
    // now is heard after `latency`. Skip that much audio.
    let behind = anchor.elapsed() + latency + Duration::from_millis(10);
    let mut skip = (behind.as_secs_f64() * RATE as f64) as usize;
    let mut pending: Vec<f32> = chunk.clone();
    let mut started = false;
    let mut ended = false;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        // Drop the samples the video has already moved past.
        while skip > 0 && !ended {
            let have = pending.len() / CHANNELS;
            if have == 0 {
                if read_frames(&mut pcm, &mut pending, skip.min(4800)) == 0 {
                    ended = true;
                }
                continue;
            }
            let n = have.min(skip);
            pending.drain(..n * CHANNELS);
            skip -= n;
        }
        // SAFETY: GetCurrentPadding/GetBuffer/ReleaseBuffer on a valid client;
        // we write at most the free space.
        unsafe {
            let padding = client.GetCurrentPadding()? as usize;
            let free = buffer_frames.saturating_sub(padding);
            if free > 0 && !(ended && pending.is_empty()) {
                if pending.len() / CHANNELS < free && !ended {
                    let mut more = Vec::new();
                    if read_frames(&mut pcm, &mut more, free - pending.len() / CHANNELS) == 0 {
                        ended = true;
                    }
                    pending.extend_from_slice(&more);
                }
                let n = (pending.len() / CHANNELS).min(free);
                if n > 0 {
                    let buf = render.GetBuffer(n as u32)?;
                    let dst = std::slice::from_raw_parts_mut(buf as *mut f32, n * CHANNELS);
                    let vol = f32::from_bits(volume.load(Ordering::Relaxed));
                    for (d, s) in dst.iter_mut().zip(pending.drain(..n * CHANNELS)) {
                        *d = s * vol;
                    }
                    render.ReleaseBuffer(n as u32, 0)?;
                    played.fetch_add(n as u64, Ordering::Relaxed);
                }
            }
            if !started {
                client.Start()?;
                started = true;
            }
            if ended && pending.is_empty() && client.GetCurrentPadding()? == 0 {
                break; // played everything
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // SAFETY: valid client; stopping a stopped client is harmless.
    unsafe {
        let _ = client.Stop();
    }
    Ok(())
}
