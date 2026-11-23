//! Where frames come from: the screen or one app's window (WGC), or a
//! synthetic test pattern.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::HWND;

use crate::capture::convert::Nv12Converter;
use crate::capture::d3d::D3D;
use crate::capture::screenshot::{BgraImage, frame_texture};
use crate::capture::wgc::WgcCapture;
use crate::capture::window;
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

/// What to capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// A monitor (HMONITOR as an integer).
    Monitor(isize),
    /// Only this app's window, found by exe name.
    Window(WindowTarget),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowTarget {
    /// `game.exe`
    pub exe: String,
    /// Preferred when the app has several windows.
    pub title: String,
}

pub trait FrameSource: Send {
    /// Output (encoder) size; fixed for the life of the source.
    fn size(&self) -> (u32, u32);
    /// Called exactly once per pacer tick.
    fn produce(&mut self, tick: u64, out: &mut Nv12Frame) -> Result<Produced>;
    /// Moves capture to another monitor without interrupting the stream
    /// (follow focus; ignored while capturing one app).
    fn switch_monitor(&mut self, _monitor: isize) -> Result<()> {
        Ok(())
    }
    /// Changes what is captured (a monitor or one app) without
    /// interrupting the stream.
    fn set_source(&mut self, _source: Source) -> Result<()> {
        Ok(())
    }
    fn current_monitor(&self) -> isize {
        0
    }
    /// Capturing one app that isn't on screen (closed, minimized, hidden or not responding): the
    /// stream is black until it's back.
    fn waiting(&self) -> bool {
        false
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

/// How often to look for the chosen app's window while it isn't there.
const LOOK_EVERY: Duration = Duration::from_secs(2);
/// How often to check that the captured window is still showing (hidden,
/// hung or on another desktop: see `window::not_showing`).
const CHECK_EVERY: Duration = Duration::from_millis(250);
/// A window that gives no picture this long after capture started is
/// recorded as black (so the buffer still starts).
const FIRST_FRAME_WAIT: Duration = Duration::from_secs(1);
/// A window Windows calls "not responding" is gone only once it has also
/// given no new picture for this long: games busy loading (Fortnite) stop
/// answering messages for seconds at a time while they keep drawing.
const FROZEN_AFTER: Duration = Duration::from_secs(2);

/// Live screen capture: WGC -> GPU NV12 conversion -> readback.
pub struct WgcSource {
    d3d: D3D,
    /// None while the chosen app isn't running.
    capture: Option<WgcCapture>,
    converter: Option<Nv12Converter>,
    out_w: u32,
    out_h: u32,
    capture_cursor: bool,
    switched_at: Option<Instant>,
    /// Some: capture only this app's window.
    window: Option<WindowTarget>,
    /// The window being captured (0 = none).
    hwnd: isize,
    next_look: Instant,
    next_check: Instant,
    attached_at: Instant,
    /// A real frame arrived since attaching.
    got_frame: bool,
    /// When the capture last gave a new picture (or attached).
    last_frame_at: Instant,
    /// The window let go of because it froze (not responding, no new
    /// picture): not attached again until it responds (0 = none).
    frozen: isize,
    /// The last frame handed out is our black frame.
    black: bool,
    /// Said once per app: it fills its monitor / gives no picture.
    told_fullscreen: bool,
    told_no_picture: bool,
}

impl WgcSource {
    pub fn new(source: Source, out_w: u32, out_h: u32, capture_cursor: bool) -> Result<Self> {
        let d3d = D3D::new()?;
        let now = Instant::now();
        let mut s = Self {
            d3d,
            capture: None,
            converter: None,
            out_w,
            out_h,
            capture_cursor,
            switched_at: None,
            window: None,
            hwnd: 0,
            next_look: now,
            next_check: now,
            attached_at: now,
            got_frame: false,
            last_frame_at: now,
            frozen: 0,
            black: false,
            told_fullscreen: false,
            told_no_picture: false,
        };
        match source {
            Source::Monitor(m) => s.capture = Some(WgcCapture::start(&s.d3d, m, capture_cursor)?),
            Source::Window(t) => {
                log::info!("capturing only {} ({:?})", t.exe, t.title);
                s.window = Some(t);
                s.watch_window();
            }
        }
        Ok(s)
    }

    /// Window mode: notices the window closing, or no longer showing
    /// (hidden, hung, another desktop: checked every CHECK_EVERY), and looks
    /// for it again (every LOOK_EVERY) while it isn't there. A window that
    /// is only covered, unfocused or static keeps being recorded.
    fn watch_window(&mut self) {
        let Some(t) = &self.window else { return };
        let now = Instant::now();
        if let Some(c) = &self.capture {
            let why = if c.is_closed() {
                Some("closed")
            } else if now >= self.next_check {
                self.next_check = now + CHECK_EVERY;
                match window::not_showing(self.hwnd) {
                    // Busy, not frozen: it still draws.
                    Some(window::NOT_RESPONDING) if self.last_frame_at.elapsed() < FROZEN_AFTER => None,
                    why => why,
                }
            } else {
                None
            };
            let Some(why) = why else { return };
            if why == window::NOT_RESPONDING {
                self.frozen = self.hwnd;
                log::info!("{} {why} and gave no new picture for {} s; recording black until it's back", t.exe, FROZEN_AFTER.as_secs());
            } else {
                log::info!("{} {why}; recording black until it's back", t.exe);
            }
            self.capture = None;
            self.hwnd = 0;
            self.next_look = now;
        }
        if now < self.next_look {
            return;
        }
        self.next_look = Instant::now() + LOOK_EVERY;
        let Some(hwnd) = window::find(&t.exe, &t.title) else { return };
        if hwnd == self.frozen && window::is_hung(hwnd) {
            return; // still frozen
        }
        self.frozen = 0;
        match WgcCapture::start_for_window(&self.d3d, hwnd, self.capture_cursor) {
            Ok(c) => {
                log::info!("found {}: {:?}", t.exe, window::title(HWND(hwnd as *mut _)));
                if window::covers_monitor(hwnd) && !self.told_fullscreen {
                    self.told_fullscreen = true;
                    log::info!(
                        "{} fills its monitor; if its clips come out black it runs in exclusive fullscreen: \
                         switch the game to borderless or windowed",
                        t.exe
                    );
                }
                self.capture = Some(c);
                self.hwnd = hwnd;
                self.next_check = Instant::now() + CHECK_EVERY;
                self.attached_at = Instant::now();
                self.last_frame_at = self.attached_at;
                self.got_frame = false;
                self.converter = None;
                self.switched_at = Some(Instant::now());
            }
            Err(e) => log::warn!("can't capture {}: {e:#}", t.exe),
        }
    }
}

/// Black in BT.709 limited range: Y = 16, U = V = 128.
fn fill_black(out: &mut Nv12Frame) {
    let luma = (out.width * out.height) as usize;
    out.data[..luma].fill(16);
    out.data[luma..].fill(128);
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
        if filled {
            self.black = false;
        }

        if self.window.is_some() {
            // 2a. One app: black while it's not showing (closed, minimized, hidden, hung), and when
            // it gives no picture at all (exclusive fullscreen, protected).
            self.watch_window();
            let hidden = self.capture.is_none() || window::is_minimized(self.hwnd);
            let no_picture = !hidden && !self.got_frame && self.attached_at.elapsed() > FIRST_FRAME_WAIT;
            if no_picture && !self.told_no_picture {
                self.told_no_picture = true;
                log::warn!("no picture from the captured window yet (exclusive fullscreen or protected content?)");
            }
            if hidden {
                if let Some(f) = self.capture.as_ref().and_then(|c| c.take_latest()) {
                    let _ = f.Close();
                }
                self.converter = None;
            }
            if (hidden || no_picture) && !self.black {
                self.black = true;
                fill_black(out);
                return Ok(Produced::New);
            }
            if hidden {
                return Ok(Produced::Unchanged);
            }
        } else if self.capture.as_ref().is_none_or(|c| c.is_closed()) {
            // 2b. Monitor unplugged? Fall back to the primary one.
            let primary = crate::capture::primary_monitor().context("no monitors left")?;
            log::warn!("captured monitor disappeared; switching to primary {}", primary.device_name);
            self.switch_monitor(primary.handle)?;
        }

        // 3. Queue the newest captured frame (if the screen changed).
        if let Some(frame) = self.capture.as_ref().and_then(|c| c.take_latest()) {
            let (tex, w, h) = frame_texture(&frame)?;
            let rebuild = self.converter.as_ref().is_none_or(|c| c.in_w != w || c.in_h != h);
            if rebuild {
                self.converter = Some(Nv12Converter::new(&self.d3d, w, h, self.out_w, self.out_h)?);
            }
            if let Some(c) = self.converter.as_mut() {
                c.submit(&tex, w, h)?;
            }
            let _ = frame.Close();
            self.got_frame = true;
            self.last_frame_at = Instant::now();
            if let Some(t) = self.switched_at.take() {
                log::info!("capture switch: first new frame after {} ms", t.elapsed().as_millis());
            }
        }
        Ok(if filled { Produced::New } else { Produced::Unchanged })
    }

    fn switch_monitor(&mut self, monitor: isize) -> Result<()> {
        if self.window.is_some() {
            return Ok(());
        }
        if self.capture.as_ref().is_some_and(|c| c.monitor == monitor && !c.is_closed()) {
            return Ok(());
        }
        // Start the new session first; until it delivers a frame, the pacer
        // keeps repeating the last frame of the old monitor, so the stream
        // never shows black or garbage during a switch.
        let new = WgcCapture::start(&self.d3d, monitor, self.capture_cursor)?;
        self.capture = Some(new);
        self.switched_at = Some(Instant::now());
        Ok(())
    }

    fn set_source(&mut self, source: Source) -> Result<()> {
        match source {
            Source::Monitor(m) => {
                if self.window.take().is_some() {
                    log::info!("capturing monitors again");
                    self.capture = None; // switch_monitor starts a monitor session
                    self.hwnd = 0;
                }
                self.switch_monitor(m)
            }
            Source::Window(t) => {
                if self.window.as_ref() == Some(&t) {
                    return Ok(());
                }
                log::info!("capturing only {} ({:?})", t.exe, t.title);
                self.window = Some(t);
                self.capture = None;
                self.hwnd = 0;
                self.frozen = 0;
                self.next_look = Instant::now();
                self.told_fullscreen = false;
                self.told_no_picture = false;
                self.watch_window();
                Ok(())
            }
        }
    }

    fn current_monitor(&self) -> isize {
        match &self.capture {
            Some(c) if self.window.is_some() => crate::capture::monitor_of_window(HWND(c.window as *mut _)),
            Some(c) => c.monitor,
            None => 0,
        }
    }

    fn waiting(&self) -> bool {
        self.window.is_some() && (self.capture.is_none() || window::is_minimized(self.hwnd))
    }

    fn latency_ticks(&self) -> u32 {
        1 // GPU readback is pipelined by one frame (see capture/convert.rs)
    }

    fn screenshot(&mut self) -> Result<BgraImage> {
        if self.waiting() {
            bail!("{} isn't on screen", self.window.as_ref().map(|t| t.exe.as_str()).unwrap_or("the app"));
        }
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
