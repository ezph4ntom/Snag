//! The "clip saved" sound.
//!
//! Uses Win32 `PlaySoundW` from memory instead of an audio library: it needs
//! no audio thread or open output stream, so it costs nothing while idle.
//! Volume is applied by scaling the WAV samples before playing.
//!
//! The sound can be any audio file (WAV, MP3, OGG, FLAC, M4A...): plain WAV
//! is read directly, anything else is converted once with ffmpeg and the
//! result kept in memory until the file changes.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_FLAGS, SND_MEMORY, SND_NODEFAULT};
use windows::core::PCWSTR;

/// Fallback if `assets/sounds/clip.wav` is missing or unreadable.
static EMBEDDED_CLIP_WAV: &[u8] = include_bytes!("../assets/sounds/clip.wav");

/// The buffer PlaySound is currently reading from. It must stay alive until
/// playback ends, so it lives here until the next sound replaces it.
static PLAYING: Mutex<Option<Vec<u8>>> = Mutex::new(None);

/// A decoded sound file: (path, modified time, WAV bytes).
type Decoded = (PathBuf, Option<SystemTime>, Vec<u8>);

/// The last decoded sound file (decoding an MP3 takes ffmpeg a moment).
static DECODED: Mutex<Option<Decoded>> = Mutex::new(None);

/// The sound file in use: the one picked in Settings, else
/// `assets/sounds/clip.wav`.
pub fn sound_path(custom: Option<&Path>) -> PathBuf {
    custom.map(Path::to_path_buf).unwrap_or_else(crate::paths::clip_sound_file)
}

/// Plays the clip sound at `volume` (0..=1). Falls back to the built-in
/// sound if the file can't be read (and logs why).
pub fn play_clip_sound(custom: Option<&Path>, volume: f32) {
    let path = sound_path(custom);
    let wav = match load(&path) {
        Ok(w) => w,
        Err(e) => {
            log::warn!("clip sound {}: {e:#}; using the built-in sound", path.display());
            EMBEDDED_CLIP_WAV.to_vec()
        }
    };
    match scale_wav(&wav, volume) {
        Ok(scaled) => play_wav_bytes(scaled),
        Err(e) => log::warn!("clip sound: {e:#}"),
    }
}

/// Reads `path` as a playable WAV, converting other formats with ffmpeg.
/// Errors say why the file can't be used (the Settings tab shows them).
pub fn load(path: &Path) -> Result<Vec<u8>> {
    let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    if let Some((p, t, wav)) = DECODED.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
        if p == path && *t == mtime {
            return Ok(wav.clone());
        }
    }
    let bytes = std::fs::read(path).with_context(|| format!("can't read {}", path.display()))?;
    // Plain PCM/float WAV: use as is. Anything else (MP3 renamed to .wav,
    // compressed WAV, OGG...): let ffmpeg convert it.
    let wav = if is_plain_wav(&bytes) { bytes } else { convert_with_ffmpeg(path)? };
    *DECODED.lock().unwrap_or_else(|p| p.into_inner()) = Some((path.to_path_buf(), mtime, wav.clone()));
    Ok(wav)
}

fn is_plain_wav(bytes: &[u8]) -> bool {
    hound::WavReader::new(Cursor::new(bytes)).is_ok_and(|r| r.spec().bits_per_sample <= 32)
}

fn convert_with_ffmpeg(path: &Path) -> Result<Vec<u8>> {
    let out = crate::win::hidden_command(crate::paths::ffmpeg_exe())
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        // A notification sound: first 10 s at most, 16-bit stereo 44.1 kHz.
        .args(["-t", "10", "-vn", "-ac", "2", "-ar", "44100", "-c:a", "pcm_s16le", "-f", "wav", "pipe:1"])
        .output()
        .context("couldn't run ffmpeg to read the sound file")?;
    if !out.status.success() || out.stdout.len() < 100 {
        let why = String::from_utf8_lossy(&out.stderr);
        bail!("not an audio file Snag can read ({})", why.lines().last().unwrap_or("no audio").trim());
    }
    // ffmpeg writes a streaming WAV header with unknown sizes; fix them so
    // PlaySound and hound accept it.
    let mut wav = out.stdout;
    let len = wav.len() as u32;
    if wav.len() >= 44 && &wav[0..4] == b"RIFF" {
        wav[4..8].copy_from_slice(&(len - 8).to_le_bytes());
        if let Some(pos) = wav.windows(4).position(|w| w == b"data") {
            let data_len = len - (pos as u32 + 8);
            wav[pos + 4..pos + 8].copy_from_slice(&data_len.to_le_bytes());
        }
    }
    Ok(wav)
}

fn play_wav_bytes(wav: Vec<u8>) {
    let mut slot = PLAYING.lock().unwrap_or_else(|p| p.into_inner());
    // SAFETY: stop any current sound first so the old buffer can be freed.
    unsafe {
        let _ = PlaySoundW(PCWSTR::null(), None, SND_FLAGS(0));
    }
    let data = slot.insert(wav);
    // SAFETY: with SND_MEMORY the "name" pointer is the WAV image; `data`
    // stays alive in PLAYING until the next call stops playback first.
    unsafe {
        let _ = PlaySoundW(PCWSTR(data.as_ptr() as *const u16), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
    }
}

/// Re-encodes a WAV with every sample multiplied by a volume factor.
/// Uses a squared curve so the slider feels even (hearing is logarithmic).
fn scale_wav(bytes: &[u8], volume: f32) -> Result<Vec<u8>> {
    let gain = volume.clamp(0.0, 1.0).powi(2);
    let mut reader = hound::WavReader::new(Cursor::new(bytes))?;
    let spec = reader.spec();
    let mut out = Cursor::new(Vec::with_capacity(bytes.len()));
    {
        let mut writer = hound::WavWriter::new(&mut out, spec)?;
        match spec.sample_format {
            hound::SampleFormat::Float => {
                for s in reader.samples::<f32>() {
                    writer.write_sample(s? * gain)?;
                }
            }
            hound::SampleFormat::Int => {
                let max = (1i64 << (spec.bits_per_sample - 1)) - 1;
                for s in reader.samples::<i32>() {
                    let v = (s? as f32 * gain).round() as i64;
                    writer.write_sample(v.clamp(-max - 1, max) as i32)?;
                }
            }
        }
        writer.finalize()?;
    }
    Ok(out.into_inner())
}

#[cfg(test)]
mod tests {
    #[test]
    fn mp3_and_renamed_files_are_converted() {
        // An MP3 saved under a .wav name (what testers tend to do).
        let dir = std::env::temp_dir().join(format!("snag-sound-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mp3 = dir.join("clip.wav");
        let ok = std::process::Command::new(crate::paths::ffmpeg_exe())
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=880:duration=0.5", "-f", "mp3"])
            .arg(&mp3)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("ffmpeg not available; skipping");
            return;
        }
        let wav = super::load(&mp3).expect("an MP3 named .wav still loads");
        let reader = hound::WavReader::new(std::io::Cursor::new(&wav)).unwrap();
        assert_eq!(reader.spec().channels, 2);
        assert!(reader.duration() > 44_100 / 3, "about 0.5 s of audio");
        assert!(super::scale_wav(&wav, 0.5).is_ok());
        let junk = dir.join("junk.mp3");
        std::fs::write(&junk, b"not audio").unwrap();
        assert!(super::load(&junk).is_err(), "garbage is reported, not silently replaced");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn embedded_sound_scales() {
        let half = super::scale_wav(super::EMBEDDED_CLIP_WAV, 0.5).unwrap();
        let reader = hound::WavReader::new(std::io::Cursor::new(&half)).unwrap();
        assert_eq!(reader.spec().sample_rate, 44_100);
        assert!(reader.len() > 1000);
    }
}
