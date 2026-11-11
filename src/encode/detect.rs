//! Finds out which encoders actually work on this machine.
//!
//! An encoder being compiled into ffmpeg doesn't mean the hardware exists
//! (this ffmpeg build includes NVENC, AMF and QSV everywhere). So we run a
//! tiny real encode with the exact arguments recording would use; if that
//! succeeds, the encoder (and our option names) are good.

use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::EncoderKind;
use super::ffmpeg::{encoder_adapter, input_path_args, video_codec_args};

/// First line of `ffmpeg -version`, or an error if ffmpeg can't be run.
pub fn ffmpeg_version() -> Result<String> {
    let out = crate::win::hidden_command(crate::paths::ffmpeg_exe())
        .arg("-version")
        .output()
        .with_context(|| format!("could not run {}", crate::paths::ffmpeg_exe().display()))?;
    if !out.status.success() {
        bail!("ffmpeg -version failed");
    }
    Ok(String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("ffmpeg").to_string())
}

/// Encodes 10 frames of a test pattern. ~0.3-1 s.
fn run_probe(kind: EncoderKind, gpu_upload: bool) -> bool {
    let (hw_args, filters) = input_path_args(kind, gpu_upload);
    let mut cmd = crate::win::hidden_command(crate::paths::ffmpeg_exe());
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"])
        .args(&hw_args)
        .args(["-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30"])
        .args(["-frames:v", "10", "-vf", &format!("format=nv12,{filters}")])
        .args(video_codec_args(kind, 30, 2000, 1, true))
        .args(["-f", "null", "-"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let Ok(mut child) = cmd.spawn() else {
        return false;
    };
    matches!(crate::win::wait_timeout(&mut child, Duration::from_secs(15)), Some(s) if s.success())
}

/// Result of probing one encoder.
#[derive(Clone, Copy, Debug)]
pub struct Probe {
    pub kind: EncoderKind,
    pub works: bool,
    /// The lighter D3D11 frame-upload path works too (NVENC/AMF).
    pub gpu_upload: bool,
}

pub fn probe_encoder(kind: EncoderKind) -> Probe {
    let gpu_upload = encoder_adapter(kind).is_some() && run_probe(kind, true);
    let works = gpu_upload || run_probe(kind, false);
    log::info!(
        "encoder probe {}: {}",
        kind.ffmpeg_codec(),
        match (works, gpu_upload) {
            (true, true) => "works (D3D11 upload)",
            (true, false) => "works",
            _ => "unavailable",
        }
    );
    Probe { kind, works, gpu_upload }
}

/// Probes every encoder in parallel (about a second total), best first.
pub fn probe_all() -> Vec<Probe> {
    let handles: Vec<_> = EncoderKind::ALL
        .iter()
        .map(|&k| std::thread::spawn(move || probe_encoder(k)))
        .collect();
    handles.into_iter().filter_map(|h| h.join().ok()).collect()
}

/// All working encoders, in preference order (hardware first).
pub fn available_encoders() -> Vec<EncoderKind> {
    probe_all().into_iter().filter(|p| p.works).map(|p| p.kind).collect()
}
