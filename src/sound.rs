//! The "clip saved" sound.
//!
//! Uses Win32 `PlaySoundW` from memory instead of an audio library: it needs
//! no audio thread or open output stream, so it costs nothing while idle.
//! Volume is applied by scaling the WAV samples before playing.

use std::io::Cursor;
use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_FLAGS, SND_MEMORY, SND_NODEFAULT};
use windows::core::PCWSTR;

/// Fallback if `assets/sounds/clip.wav` is missing or unreadable.
static EMBEDDED_CLIP_WAV: &[u8] = include_bytes!("../assets/sounds/clip.wav");

/// The buffer PlaySound is currently reading from. It must stay alive until
/// playback ends, so it lives here until the next sound replaces it.
static PLAYING: Mutex<Option<Vec<u8>>> = Mutex::new(None);

/// Plays the clip sound from `assets/sounds/clip.wav` at `volume` (0..=1).
pub fn play_clip_sound(volume: f32) {
    let path = crate::paths::clip_sound_file();
    if let Err(e) = play_wav_file(&path, volume) {
        log::warn!("clip sound: {e:#}");
    }
}

pub fn play_wav_file(path: &Path, volume: f32) -> Result<()> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            log::warn!("{} unreadable ({e}); using built-in sound", path.display());
            EMBEDDED_CLIP_WAV.to_vec()
        }
    };
    let scaled = scale_wav(&bytes, volume).or_else(|e| {
        log::warn!("{} is not a supported WAV ({e}); using built-in sound", path.display());
        scale_wav(EMBEDDED_CLIP_WAV, volume)
    })?;
    play_wav_bytes(scaled);
    Ok(())
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
    fn embedded_sound_scales() {
        let half = super::scale_wav(super::EMBEDDED_CLIP_WAV, 0.5).unwrap();
        let reader = hound::WavReader::new(std::io::Cursor::new(&half)).unwrap();
        assert_eq!(reader.spec().sample_rate, 44_100);
        assert!(reader.len() > 1000);
    }
}
