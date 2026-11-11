//! Preview decoding for the trimmer: an ffmpeg process decodes from a given
//! frame onward into small RGBA images, which a reader thread hands to the
//! UI through a short queue. The queue being short (bounded) makes ffmpeg
//! pause when the UI isn't consuming, so a paused preview costs no CPU.

use std::io::Read;
use std::process::{Child, Stdio};
use std::sync::mpsc::{Receiver, TryRecvError, sync_channel};
use std::thread::JoinHandle;

use anyhow::{Context, Result};

use super::ClipProbe;

pub struct Frame {
    pub index: usize,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub struct Decoder {
    child: Child,
    rx: Option<Receiver<Frame>>,
    thread: Option<JoinHandle<()>>,
    /// Frame index the decoder started at.
    pub start: usize,
    finished: bool,
}

/// Preview size: fits in 960x540, keeps aspect ratio, even dimensions.
pub fn preview_size(w: u32, h: u32) -> (u32, u32) {
    let (w, h) = (w.max(2) as f64, h.max(2) as f64);
    let scale = (960.0 / w).min(540.0 / h).min(1.0);
    (((w * scale) as u32).max(2) & !1, ((h * scale) as u32).max(2) & !1)
}

impl Decoder {
    pub fn start(probe: &ClipProbe, start: usize, size: (u32, u32)) -> Result<Decoder> {
        let (w, h) = size;
        // Half a frame early, so accurate seeking can't round past `start`.
        let t = probe.seek_time(start, -probe.frame_duration() / 2.0);
        let frame_bytes = (w * h * 4) as usize;
        let (pipe_read, pipe_write) = crate::win::big_pipe((frame_bytes * 2) as u32)?;
        let mut cmd = crate::win::hidden_command(crate::paths::ffmpeg_exe());
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"])
            // H.264 frame threading keeps a set of frames per thread; 4 threads
            // decode 1080p far faster than real time and keep memory small.
            .args(["-threads", "4"])
            .args(["-ss", &format!("{t:.6}")])
            .arg("-i")
            .arg(&probe.path)
            .args(["-an", "-sn", "-vf", &format!("scale={w}:{h}:flags=bilinear")])
            .args(["-fps_mode", "passthrough", "-pix_fmt", "rgba", "-f", "rawvideo", "pipe:1"])
            .stdin(Stdio::null())
            .stdout(Stdio::from(pipe_write))
            .stderr(Stdio::null());
        let child = cmd.spawn().context("could not start ffmpeg for preview")?;
        drop(cmd); // release our copy of the pipe's write end

        let (tx, rx) = sync_channel::<Frame>(4);
        let thread = std::thread::spawn(move || {
            let mut reader = pipe_read;
            let mut index = start;
            loop {
                let mut rgba = vec![0u8; frame_bytes];
                if reader.read_exact(&mut rgba).is_err() {
                    break; // end of clip or decoder killed
                }
                if tx.send(Frame { index, width: w, height: h, rgba }).is_err() {
                    break; // UI dropped the decoder
                }
                index += 1;
            }
        });
        Ok(Decoder {
            child,
            rx: Some(rx),
            thread: Some(thread),
            start,
            finished: false,
        })
    }

    /// Next decoded frame, if one is ready.
    pub fn try_next(&mut self) -> Option<Frame> {
        match self.rx.as_ref()?.try_recv() {
            Ok(f) => Some(f),
            Err(TryRecvError::Disconnected) => {
                self.finished = true;
                None
            }
            Err(TryRecvError::Empty) => None,
        }
    }

    /// True once the decoder reached the end of the clip.
    pub fn finished(&self) -> bool {
        self.finished
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        drop(self.rx.take()); // unblocks the reader if it waits on a full queue
        let _ = self.child.wait();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
