//! WASAPI capture of one playback device (loopback: what it plays) or one
//! microphone (`LoopbackSource::microphone`: what it hears).
//!
//! We ask Windows for 48 kHz stereo float and let it convert from whatever
//! the device runs at (`AUTOCONVERTPCM`). A source made with `new()`
//! follows the default device (re-checked every 2 seconds, so switching from
//! speakers to a headset mid-session just works); `for_device` sticks to one
//! device. If the device is missing, nothing is captured and the recorder
//! writes silence instead, so the clip still has an audio track.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::Win32::Media::Audio::{EDataFlow, eCapture,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioCaptureClient, IAudioClient,
    IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX, eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree};

use super::{AudioPacket, AudioSource, CHANNELS, SAMPLE_RATE};

/// WAVE_FORMAT_IEEE_FLOAT (from mmreg.h).
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

struct Capture {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    device_id: String,
}

impl Drop for Capture {
    fn drop(&mut self) {
        // SAFETY: valid client; stopping twice is harmless.
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

pub struct LoopbackSource {
    /// `None`: follow the default device.
    device: Option<String>,
    /// A microphone (input device) instead of a playback device.
    mic: bool,
    capture: Option<Capture>,
    last_device_check: Instant,
    /// Log "no audio device" only once, not every 2 seconds.
    reported_failure: bool,
}

// SAFETY: created and used only on the recorder's audio thread (the factory
// runs there); the COM objects never cross threads.
unsafe impl Send for LoopbackSource {}

fn enumerator() -> Result<IMMDeviceEnumerator> {
    // SAFETY: standard COM activation (COM is initialized by the audio thread).
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).context("audio device enumerator") }
}

fn flow(mic: bool) -> EDataFlow {
    if mic { eCapture } else { eRender }
}

fn default_device_id(mic: bool) -> Result<String> {
    // SAFETY: COM calls on valid interfaces; the id string is freed below.
    unsafe {
        let device = enumerator()?.GetDefaultAudioEndpoint(flow(mic), eConsole)?;
        let pwstr = device.GetId()?;
        let id = pwstr.to_string().unwrap_or_default();
        CoTaskMemFree(Some(pwstr.0 as *const _));
        Ok(id)
    }
}

fn open(wanted: Option<&str>, mic: bool) -> Result<Capture> {
    let what = if mic { "microphone" } else { "playback device" };
    // SAFETY: standard WASAPI setup; `format` outlives Initialize.
    unsafe {
        let device = match wanted {
            None => enumerator()?.GetDefaultAudioEndpoint(flow(mic), eConsole).with_context(|| format!("no {what}"))?,
            Some(id) => {
                let wide = crate::win::to_wide(id);
                enumerator()?.GetDevice(windows::core::PCWSTR(wide.as_ptr())).with_context(|| format!("{what} is gone"))?
            }
        };
        let pwstr = device.GetId()?;
        let device_id = pwstr.to_string().unwrap_or_default();
        CoTaskMemFree(Some(pwstr.0 as *const _));

        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let block = (CHANNELS * 4) as u16;
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            nChannels: CHANNELS as u16,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * block as u32,
            nBlockAlign: block,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        // A microphone is captured directly; a playback device via loopback.
        // AUTOCONVERTPCM also turns a mono microphone into stereo.
        let loopback = if mic { Default::default() } else { AUDCLNT_STREAMFLAGS_LOOPBACK };
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                loopback | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                2_000_000, // 200 ms buffer, in 100 ns units
                0,
                &format,
                None,
            )
            .with_context(|| format!("starting {what} capture"))?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        log::info!("audio: capturing {} {what}", if wanted.is_some() { "a chosen" } else { "the default" });
        Ok(Capture { client, capture, device_id })
    }
}

impl LoopbackSource {
    pub fn new() -> LoopbackSource {
        Self::make(None)
    }

    /// Captures the device with this id (from `devices::render_devices`).
    pub fn for_device(id: &str) -> LoopbackSource {
        Self::make(Some(id.to_string()))
    }

    /// Captures a microphone (`None`: the default one, followed if it
    /// changes).
    pub fn microphone(id: Option<&str>) -> LoopbackSource {
        Self::build(id.map(str::to_string), true)
    }

    fn make(device: Option<String>) -> LoopbackSource {
        Self::build(device, false)
    }

    fn build(device: Option<String>, mic: bool) -> LoopbackSource {
        let mut s = LoopbackSource {
            device,
            mic,
            capture: None,
            last_device_check: Instant::now(),
            reported_failure: false,
        };
        s.reopen();
        s
    }

    fn reopen(&mut self) {
        self.capture = None;
        match open(self.device.as_deref(), self.mic) {
            Ok(c) => {
                self.capture = Some(c);
                self.reported_failure = false;
            }
            Err(e) => {
                if !self.reported_failure {
                    log::warn!("PC audio unavailable ({e:#}); recording silence until a device appears");
                    self.reported_failure = true;
                }
            }
        }
    }

    fn drain(&mut self, out: &mut Vec<AudioPacket>) -> Result<()> {
        let Some(c) = &self.capture else { return Ok(()) };
        // SAFETY: GetBuffer/ReleaseBuffer pairs on a started capture client;
        // we copy the data out before releasing the buffer.
        unsafe {
            loop {
                if c.capture.GetNextPacketSize()? == 0 {
                    return Ok(());
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                let mut qpc = 0u64;
                c.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
                let n = frames as usize * CHANNELS;
                let samples = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    vec![0.0f32; n]
                } else {
                    std::slice::from_raw_parts(data as *const f32, n).to_vec()
                };
                c.capture.ReleaseBuffer(frames)?;
                out.push(AudioPacket { qpc_100ns: qpc, samples, stream: 0 });
            }
        }
    }
}

impl Default for LoopbackSource {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioSource for LoopbackSource {
    fn poll(&mut self, out: &mut Vec<AudioPacket>, _start: u64, _now: u64) {
        // Follow the default device (headset plugged in, output switched...).
        if self.device.is_none() && self.last_device_check.elapsed() >= Duration::from_secs(2) {
            self.last_device_check = Instant::now();
            let current = default_device_id(self.mic).ok();
            let ours = self.capture.as_ref().map(|c| c.device_id.clone());
            if current != ours {
                if current.is_some() {
                    log::info!("PC audio: default playback device changed");
                }
                self.reopen();
            }
        }
        if let Err(e) = self.drain(out) {
            log::warn!("PC audio capture interrupted ({e:#}); reopening");
            self.reopen();
        }
    }
}
