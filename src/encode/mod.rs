//! Video encoding.
//!
//! The rest of the app talks to encoders only through [`VideoEncoder`], so
//! the v1 ffmpeg sidecar ([`ffmpeg::FfmpegHlsEncoder`]) can later be replaced
//! by an in-process encoder (Media Foundation, NVENC SDK, AMF SDK) that takes
//! GPU textures directly and skips the readback + pipe.
//!
//! **Output contract** for any implementation (the replay buffer relies on it):
//! in `EncoderConfig::segment_dir`, write
//! * `init.mp4`: fragmented-MP4 init segment (codec headers),
//! * `seg_NNNNNN.m4s`: media segments of exactly `segment_seconds` each,
//!   numbered from 0, each starting with an IDR keyframe, only renamed to
//!   that name once complete,
//! * `buffer.m3u8`: HLS playlist listing the complete segments.
//!
//! Segments may also contain an AAC audio track next to the video track.
//!
//! Concatenating `init.mp4` + any run of consecutive segments is a valid,
//! playable fragmented MP4, which is how clips are cut without re-encoding.

pub mod detect;
pub mod ffmpeg;

use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum EncoderKind {
    /// NVIDIA NVENC (GeForce GTX 600+ / RTX).
    Nvenc,
    /// AMD Advanced Media Framework (Radeon).
    Amf,
    /// Intel Quick Sync Video (Intel iGPU / Arc).
    Qsv,
    /// x264 on the CPU. Always available, but costs CPU the game would use.
    X264,
}

impl EncoderKind {
    /// Preference order when auto-selecting.
    pub const ALL: [EncoderKind; 4] = [Self::Nvenc, Self::Amf, Self::Qsv, Self::X264];

    pub fn ffmpeg_codec(self) -> &'static str {
        match self {
            Self::Nvenc => "h264_nvenc",
            Self::Amf => "h264_amf",
            Self::Qsv => "h264_qsv",
            Self::X264 => "libx264",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Nvenc => "NVIDIA NVENC (H.264)",
            Self::Amf => "AMD AMF (H.264)",
            Self::Qsv => "Intel Quick Sync (H.264)",
            Self::X264 => "x264 software (H.264)",
        }
    }

    pub fn is_hardware(self) -> bool {
        self != Self::X264
    }
}

/// Parameters for one recording session.
#[derive(Clone, Debug)]
pub struct EncoderConfig {
    pub kind: EncoderKind,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Keyframe interval == segment length, in whole seconds (1 or 2).
    pub segment_seconds: u32,
    pub segment_dir: PathBuf,
    /// How many complete segments the encoder keeps before deleting old ones.
    pub keep_segments: u32,
    /// Upload frames to the encoder's GPU as D3D11 textures (NVENC/AMF;
    /// only if detection verified it works). See `ffmpeg::input_path_args`.
    pub gpu_upload: bool,
    /// Also record an audio track (fed through `VideoEncoder::take_audio_sink`).
    pub audio: bool,
    pub audio_bitrate_kbps: u32,
}

/// Where the recorder writes audio: interleaved stereo f32 at 48 kHz, where
/// sample 0 is the same instant as video frame 0.
pub trait AudioSink: Send {
    /// Blocks until the encoder accepts audio. Call it right away (not after
    /// the video `prime`): the encoder may need some audio to finish starting.
    /// Returns how many frames at the start of the timeline it already
    /// filled with silence (the caller continues from there).
    fn open(&mut self) -> Result<u64>;
    fn write(&mut self, samples: &[f32]) -> Result<()>;
}

/// One frame in system memory, NV12 layout: `width*height` bytes of luma,
/// then `width*height/2` bytes of interleaved U/V at half resolution.
pub struct Nv12Frame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Nv12Frame {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            data: vec![0; (width * height * 3 / 2) as usize],
        }
    }
}

/// The seam between the capture pipeline and an encoder implementation.
///
/// Frames carry no timestamps: frame `n` is at `n / fps` seconds. The pacer
/// guarantees exactly one frame per tick, which is what makes the output
/// constant-frame-rate with correct timing.
pub trait VideoEncoder: Send {
    /// Called once before the stream clock starts: blocks until the encoder
    /// is ready to accept frames in real time. Frames passed here are *not*
    /// part of the output.
    fn prime(&mut self, _frame: &Nv12Frame) -> Result<()> {
        Ok(())
    }
    /// Encodes the next frame. May block briefly if the encoder is busy.
    fn encode(&mut self, frame: &Nv12Frame) -> Result<()>;
    /// Flushes pending frames and finalizes the output.
    fn finish(&mut self) -> Result<()>;
    /// The audio input, if `EncoderConfig::audio` was set. Taken once.
    fn take_audio_sink(&mut self) -> Option<Box<dyn AudioSink>> {
        None
    }
}

/// Creates the configured encoder. Today that's always the ffmpeg sidecar.
pub fn create_encoder(cfg: &EncoderConfig) -> Result<Box<dyn VideoEncoder>> {
    Ok(Box::new(ffmpeg::FfmpegHlsEncoder::spawn(cfg)?))
}
