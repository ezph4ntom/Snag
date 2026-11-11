//! Where frames come from: the screen (WGC) or a synthetic test pattern.

use std::time::Instant;

use anyhow::{Context, Result, bail};

use crate::capture::convert::Nv12Converter;
use crate::capture::d3d::D3D;
use crate::capture::screenshot::{BgraImage, frame_texture};
use crate::capture::wgc::WgcCapture;
use crate::encode::Nv12Frame;

/// Result of asking a source for the current tick's frame.
#[derive(Debug, PartialEq, Eq)]
pub enum Produced {
    /// `out` holds a new frame.
    New,
    /// Nothing changed on screen since last tick; the pacer repeats the
    /// previous frame (that's what constant frame rate means for a static
    /// screen; it's not a drop).
    Unchanged,
}

pub trait FrameSource: Send {
    /// Output (encoder) size; fixed for the life of the source.
    fn size(&self) -> (u32, u32);
    /// Called exactly once per pacer tick.
    fn produce(&mut self, tick: u64, out: &mut Nv12Frame) -> Result<Produced>;
    /// Moves capture to another monitor without interrupting the stream.
    fn switch_monitor(&mut self, _monitor: isize) -> Result<()> {
        Ok(())
    }
    fn current_monitor(&self) -> isize {
        0
    }
    /// How many ticks late frames reach the encoder (0 = the frame produced
    /// at tick `n` shows the screen at tick `n`). Audio is delayed to match.
    fn latency_ticks(&self) -> u32 {
        0
    }
    /// Native-resolution image of the latest frame.
    fn screenshot(&mut self) -> Result<BgraImage> {
        bail!("this source cannot take screenshots")
    }
}

/// Live screen capture: WGC -> GPU NV12 conversion -> readback.
pub struct WgcSource {
    d3d: D3D,
    capture: WgcCapture,
    converter: Option<Nv12Converter>,
    out_w: u32,
    out_h: u32,
    capture_cursor: bool,
    switched_at: Option<Instant>,
}

impl WgcSource {
    pub fn new(monitor: isize, out_w: u32, out_h: u32, capture_cursor: bool) -> Result<Self> {
        let d3d = D3D::new()?;
        let capture = WgcCapture::start(&d3d, monitor, capture_cursor)?;
        Ok(Self {
            d3d,
            capture,
            converter: None,
            out_w,
            out_h,
            capture_cursor,
            switched_at: None,
        })
    }
}

impl FrameSource for WgcSource {
    fn size(&self) -> (u32, u32) {
        (self.out_w, self.out_h)
    }

    fn produce(&mut self, _tick: u64, out: &mut Nv12Frame) -> Result<Produced> {
        // 1. Collect the frame we queued on the GPU last tick (already done).
        let filled = match self.converter.as_mut() {
            Some(c) => c.read_pending(&mut out.data)?,
            None => false,
        };

        // 2. Monitor unplugged? Fall back to the primary one.
        if self.capture.is_closed() {
            let primary = crate::capture::primary_monitor().context("no monitors left")?;
            log::warn!("captured monitor disappeared; switching to primary {}", primary.device_name);
            self.switch_monitor(primary.handle)?;
        }

        // 3. Queue the newest captured frame (if the screen changed).
        if let Some(frame) = self.capture.take_latest() {
            let (tex, w, h) = frame_texture(&frame)?;
            let rebuild = self.converter.as_ref().is_none_or(|c| c.in_w != w || c.in_h != h);
            if rebuild {
                self.converter = Some(Nv12Converter::new(&self.d3d, w, h, self.out_w, self.out_h)?);
            }
            if let Some(c) = self.converter.as_mut() {
                c.submit(&tex, w, h)?;
            }
            let _ = frame.Close();
            if let Some(t) = self.switched_at.take() {
                log::info!("monitor switch: first new frame after {} ms", t.elapsed().as_millis());
            }
        }
        Ok(if filled { Produced::New } else { Produced::Unchanged })
    }

    fn switch_monitor(&mut self, monitor: isize) -> Result<()> {
        if monitor == self.capture.monitor && !self.capture.is_closed() {
            return Ok(());
        }
        // Start the new session first; until it delivers a frame, the pacer
        // keeps repeating the last frame of the old monitor, so the stream
        // never shows black or garbage during a switch.
        let new = WgcCapture::start(&self.d3d, monitor, self.capture_cursor)?;
        self.capture = new;
        self.switched_at = Some(Instant::now());
        Ok(())
    }

    fn current_monitor(&self) -> isize {
        self.capture.monitor
    }

    fn latency_ticks(&self) -> u32 {
        1 // GPU readback is pipelined by one frame (see capture/convert.rs)
    }

    fn screenshot(&mut self) -> Result<BgraImage> {
        self.converter.as_ref().context("no frame captured yet")?.read_input_bgra()
    }
}

/// Test pattern source: every tick renders a new frame whose top band is a
/// 16-bit barcode of the tick number (see `verify`). Decoding a clip and
/// reading the barcodes back proves frame-exact continuity: no frame
/// dropped, duplicated or reordered anywhere in the pipeline.
pub struct SyntheticSource {
    w: u32,
    h: u32,
}

impl SyntheticSource {
    pub fn new(w: u32, h: u32) -> Self {
        Self { w: w & !1, h: h & !1 }
    }
}

impl FrameSource for SyntheticSource {
    fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }

    fn produce(&mut self, tick: u64, out: &mut Nv12Frame) -> Result<Produced> {
        crate::verify::draw_test_frame_nv12(out, tick as u32);
        Ok(Produced::New)
    }
}
