//! PC audio ("what you hear") for clips.
//!
//! * [`mixer::MixSource`] is what recordings use: it captures what the
//!   user picked in the Sound tab (every playback device, or only some
//!   apps) and hands all of it to the recorder, which mixes it.
//! * [`loopback::LoopbackSource`] captures one playback device with WASAPI
//!   loopback: exactly the mix you hear on it.
//! * [`process::ProcessLoopbackSource`] captures one app, on any device.
//! * [`BeepSource`] is a test source: a 10 ms beep at every whole second of
//!   the recording, used to measure audio/video sync in the tests.
//!
//! Every packet carries the time its first sample was played (Windows'
//! performance counter, in 100 ns units). The recorder places packets on
//! the same clock as the video frames, which is what keeps them in sync.

pub mod devices;
pub mod loopback;
pub mod mixer;
pub mod process;

/// Audio handed to the encoder is always 48 kHz stereo, 32-bit float.
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;

/// Captured audio: interleaved stereo f32 samples (`samples.len() / 2`
/// frames), the first of which was played at `qpc_100ns`.
pub struct AudioPacket {
    pub qpc_100ns: u64,
    pub samples: Vec<f32>,
    /// Which source this came from when several are mixed (packets of one
    /// stream are contiguous; see `mixer::MixBuffer`).
    pub stream: u32,
}

pub trait AudioSource: Send {
    /// Appends everything captured since the last call. Called about every
    /// 10 ms. `start_100ns` is the moment video frame 0 was taken and
    /// `now_100ns` the current time, both on the performance-counter clock.
    fn poll(&mut self, out: &mut Vec<AudioPacket>, start_100ns: u64, now_100ns: u64);
}

/// Creates the audio source on the audio thread (WASAPI's COM objects
/// should live on the thread that uses them).
pub type AudioSourceFactory = Box<dyn FnOnce() -> Box<dyn AudioSource> + Send>;

/// The performance counter (same clock as Rust's `Instant` and as WASAPI's
/// packet timestamps), converted to 100 ns units.
pub fn qpc_now_100ns() -> u64 {
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    let (mut count, mut freq) = (0i64, 0i64);
    // SAFETY: out-params are valid; both calls always succeed on XP+.
    unsafe {
        let _ = QueryPerformanceCounter(&mut count);
        let _ = QueryPerformanceFrequency(&mut freq);
    }
    (count as i128 * 10_000_000 / freq.max(1) as i128) as u64
}

/// Converts a duration in 100 ns units to a number of audio frames.
pub fn frames_from_100ns(t: i128) -> i64 {
    (t * SAMPLE_RATE as i128 / 10_000_000) as i64
}

/// Test source: a 10 ms, 1 kHz beep starting exactly at every whole second
/// of the recording timeline, silence otherwise. Video frame `k * fps` of a
/// synthetic recording is at the same instant, so comparing beep onsets with
/// those frames in a saved clip measures audio/video sync.
#[derive(Default)]
pub struct BeepSource {
    /// Next timeline frame to generate.
    next: i64,
}

impl AudioSource for BeepSource {
    fn poll(&mut self, out: &mut Vec<AudioPacket>, start_100ns: u64, now_100ns: u64) {
        // Pretend the "device" has 20 ms of latency, like real hardware.
        let until = frames_from_100ns(now_100ns as i128 - start_100ns as i128) - 960;
        if until <= self.next {
            return;
        }
        let rate = SAMPLE_RATE as i64;
        let mut samples = Vec::with_capacity(((until - self.next) * 2) as usize);
        for i in self.next..until {
            let phase = i.rem_euclid(rate);
            let v = if phase < rate / 100 {
                0.5 * (2.0 * std::f32::consts::PI * 1000.0 * phase as f32 / rate as f32).sin()
            } else {
                0.0
            };
            samples.extend_from_slice(&[v, v]);
        }
        out.push(AudioPacket {
            qpc_100ns: start_100ns + (self.next as i128 * 10_000_000 / rate as i128) as u64,
            samples,
            stream: 0,
        });
        self.next = until;
    }
}
