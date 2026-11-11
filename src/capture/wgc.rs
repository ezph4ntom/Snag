//! One Windows Graphics Capture session for one monitor.
//!
//! WGC queues a frame in its pool whenever the screen changes (up to the
//! refresh rate). Frames are collected two ways, and both just swap the
//! frame into a one-slot mailbox holding only the newest frame:
//! * the `FrameArrived` callback (a system thread-pool thread), and
//! * the pacer itself, which drains the pool at every tick
//!   (`take_latest`). The callback can be scheduled late; without this the
//!   pacer would sometimes find no new frame although one was waiting.
//!
//! Older frames are closed immediately so their buffers go back to the pool.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem,
    GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::IInspectable;

use super::d3d::D3D;

const PIXEL_FORMAT: DirectXPixelFormat = DirectXPixelFormat::B8G8R8A8UIntNormalized;
/// Capture buffers. With 2, one sits in our mailbox and one is queued, so if
/// nobody collects in time the compositor has nowhere to write and silently
/// skips a frame. 3 leaves headroom.
const POOL_BUFFERS: i32 = 3;

/// State shared by the callback thread and the pacer thread.
struct Shared {
    latest: Mutex<Option<Direct3D11CaptureFrame>>,
    closed: AtomicBool,
    /// Size the pool's buffers were created with.
    pool_size: Mutex<SizeInt32>,
    /// The WinRT device (needed to recreate the pool on resolution change).
    device: IDirect3DDevice,
    // Diagnostics, logged when the session ends.
    arrived: AtomicU64,
    long_gaps: AtomicU64,
    max_gap_us: AtomicU64,
    last_arrival: Mutex<Option<Instant>>,
}

// SAFETY: IDirect3DDevice wraps a D3D11 device, which is free-threaded;
// windows-rs just doesn't mark the WinRT wrapper Send/Sync. Everything else
// here is Send + Sync already.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Shared {
    /// Stores `frame` as the newest frame and closes the one it replaces.
    fn accept(&self, pool: &Direct3D11CaptureFramePool, frame: Direct3D11CaptureFrame) {
        // If the monitor resolution changed, the pool's buffers are the wrong
        // size from now on. Recreate them; this frame stays valid, and the
        // converter crops it to ContentSize.
        if let (Ok(content), Ok(mut ps)) = (frame.ContentSize(), self.pool_size.lock()) {
            if content.Width != ps.Width || content.Height != ps.Height {
                let new = SizeInt32 { Width: content.Width.max(1), Height: content.Height.max(1) };
                if pool.Recreate(&self.device, PIXEL_FORMAT, POOL_BUFFERS, new).is_ok() {
                    log::info!("capture size changed to {}x{}", new.Width, new.Height);
                    *ps = new;
                }
            }
        }
        self.arrived.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut last) = self.last_arrival.lock() {
            let now = Instant::now();
            if let Some(prev) = last.replace(now) {
                let gap = (now - prev).as_micros() as u64;
                if gap > 16_667 {
                    self.long_gaps.fetch_add(1, Ordering::Relaxed);
                }
                self.max_gap_us.fetch_max(gap, Ordering::Relaxed);
            }
        }
        let previous = self.latest.lock().ok().and_then(|mut slot| slot.replace(frame));
        if let Some(old) = previous {
            let _ = old.Close(); // buffer back to the pool right away
        }
    }

    /// Moves every frame waiting in the pool into the mailbox.
    fn drain(&self, pool: &Direct3D11CaptureFramePool) {
        // TryGetNextFrame returns an error (null) once the pool is empty.
        while let Ok(frame) = pool.TryGetNextFrame() {
            self.accept(pool, frame);
        }
    }
}

pub struct WgcCapture {
    /// HMONITOR (as integer) this session captures.
    pub monitor: isize,
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    shared: Arc<Shared>,
    frame_token: i64,
    closed_token: i64,
}

impl WgcCapture {
    /// Starts capturing `monitor`. Frames begin arriving asynchronously.
    pub fn start(d3d: &D3D, monitor: isize, capture_cursor: bool) -> Result<Self> {
        if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
            anyhow::bail!("Windows Graphics Capture is not supported on this system (needs Windows 10 1903+)");
        }
        // "Interop" interfaces are how Win32 code creates WinRT capture items
        // from raw HMONITOR/HWND handles.
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        // SAFETY: monitor handle came from EnumDisplayMonitors/MonitorFromWindow.
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(HMONITOR(monitor as *mut _)) }
            .context("CreateForMonitor failed (monitor disconnected?)")?;
        let size = item.Size()?;

        // "Free-threaded" pool: no dispatcher queue / message loop needed, and
        // any thread may call TryGetNextFrame.
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&d3d.winrt_device, PIXEL_FORMAT, POOL_BUFFERS, size)?;
        let session = pool.CreateCaptureSession(&item)?;

        let shared = Arc::new(Shared {
            latest: Mutex::new(None),
            closed: AtomicBool::new(false),
            pool_size: Mutex::new(size),
            device: d3d.winrt_device.clone(),
            arrived: AtomicU64::new(0),
            long_gaps: AtomicU64::new(0),
            max_gap_us: AtomicU64::new(0),
            last_arrival: Mutex::new(None),
        });

        let sh = shared.clone();
        let frame_token = pool.FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(
            move |pool, _| {
                if let Some(pool) = pool.as_ref() {
                    sh.drain(pool);
                }
                Ok(())
            },
        ))?;

        let sh = shared.clone();
        let closed_token = item.Closed(&TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(
            move |_, _| {
                // Fires when the monitor is disconnected.
                sh.closed.store(true, Ordering::Relaxed);
                Ok(())
            },
        ))?;

        let _ = session.SetIsCursorCaptureEnabled(capture_cursor);
        // Win11: no yellow capture border. Not available on Win10 (ignored).
        let _ = session.SetIsBorderRequired(false);
        // By default WGC delivers at most ~60 frames/s whatever the refresh
        // rate. Sampling that at 60 fps with an unsynchronized clock means
        // some ticks see no new frame (a repeat). Letting WGC deliver every
        // composition (Windows 11 24H2+) gives every tick a fresh frame.
        if session
            .SetMinUpdateInterval(windows::Foundation::TimeSpan { Duration: 10_000 }) // 1 ms, in 100 ns units
            .is_err()
        {
            log::info!("MinUpdateInterval unsupported (pre-24H2 Windows): capture limited to ~60 fps");
        }
        session.StartCapture()?;
        log::info!("capture started on monitor {:#x} ({}x{})", monitor, size.Width, size.Height);

        Ok(Self {
            monitor,
            item,
            pool,
            session,
            shared,
            frame_token,
            closed_token,
        })
    }

    /// Takes the newest frame, if a new one arrived since the last call.
    /// Call `Close()` on it when done so its buffer returns to the pool.
    pub fn take_latest(&self) -> Option<Direct3D11CaptureFrame> {
        self.shared.drain(&self.pool);
        self.shared.latest.lock().ok().and_then(|mut slot| slot.take())
    }

    /// True once the captured monitor went away.
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Relaxed)
    }

    /// Total frames delivered by WGC (for stats).
    pub fn frames_arrived(&self) -> u64 {
        self.shared.arrived.load(Ordering::Relaxed)
    }
}

impl Drop for WgcCapture {
    fn drop(&mut self) {
        let _ = self.pool.RemoveFrameArrived(self.frame_token);
        let _ = self.item.RemoveClosed(self.closed_token);
        if let Some(f) = self.shared.latest.lock().ok().and_then(|mut s| s.take()) {
            let _ = f.Close();
        }
        let _ = self.session.Close();
        let _ = self.pool.Close();
        log::info!(
            "capture stopped on monitor {:#x}: {} frames delivered, {} gaps > 16.7 ms, longest gap {:.1} ms",
            self.monitor,
            self.shared.arrived.load(Ordering::Relaxed),
            self.shared.long_gaps.load(Ordering::Relaxed),
            self.shared.max_gap_us.load(Ordering::Relaxed) as f64 / 1000.0
        );
    }
}
