//! Minimal reader for the HLS playlist ffmpeg maintains for the buffer.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    /// Number from the file name (`seg_000123.m4s` -> 123). Segment `n`
    /// covers stream time `[n * len, (n + 1) * len)`.
    pub seq: u64,
    pub duration: f64,
    pub file: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Playlist {
    pub segments: Vec<Segment>,
    /// `#EXT-X-ENDLIST` present: the encoder has stopped.
    pub ended: bool,
}

impl Playlist {
    pub fn last_seq(&self) -> Option<u64> {
        self.segments.last().map(|s| s.seq)
    }
}

pub fn parse(text: &str) -> Playlist {
    let mut pl = Playlist::default();
    let mut pending_duration: Option<f64> = None;
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            pending_duration = rest.split(',').next().and_then(|d| d.parse().ok());
        } else if line == "#EXT-X-ENDLIST" {
            pl.ended = true;
        } else if !line.is_empty() && !line.starts_with('#') {
            let seq = line
                .trim_start_matches("seg_")
                .trim_end_matches(".m4s")
                .parse::<u64>();
            if let (Ok(seq), Some(duration)) = (seq, pending_duration.take()) {
                pl.segments.push(Segment {
                    seq,
                    duration,
                    file: line.to_string(),
                });
            }
        }
    }
    pl
}

/// Reads `buffer.m3u8` from a session folder. ffmpeg replaces the file by
/// rename; a read can collide with that for a moment, so retry briefly.
pub fn read(dir: &Path) -> Result<Playlist> {
    let path = dir.join(crate::encode::ffmpeg::PLAYLIST);
    let mut last_err = None;
    for _ in 0..10 {
        match std::fs::read_to_string(&path) {
            Ok(text) => return Ok(parse(&text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Playlist::default()),
            Err(e) => last_err = Some(e),
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(last_err.unwrap()).with_context(|| format!("reading {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ffmpeg_playlist() {
        let text = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:3\n\
                    #EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:1.000000,\nseg_000003.m4s\n#EXTINF:1.000000,\nseg_000004.m4s\n";
        let pl = parse(text);
        assert_eq!(pl.segments.len(), 2);
        assert_eq!(pl.segments[0].seq, 3);
        assert_eq!(pl.last_seq(), Some(4));
        assert!(!pl.ended);
        assert!(parse(&format!("{text}#EXT-X-ENDLIST\n")).ended);
    }
}
