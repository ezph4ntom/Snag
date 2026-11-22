//! Live preview: the core hands small RGBA copies of the frames it records
//! to the window process through shared memory.
//!
//! Only while the window asks for it (Live tab visible): the recorder then
//! offers every other frame (about 30 per second) to a preview thread, which
//! shrinks the encoder's NV12 frame to at most 960 pixels wide, converts it
//! to RGBA and writes it into a named shared-memory block. The window maps
//! the same block, and a small watcher thread (`watch`) wakes it only when
//! the frame counter changes.
//!
//! Layout of the block: `[seq u32][width u32][height u32][pad u32][rgba...]`.
//! `seq` is odd while a frame is being written (a "seqlock"): the reader
//! copies only when it reads the same even `seq` before and after copying.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile,
};
use windows::core::HSTRING;

use crate::encode::Nv12Frame;

/// The shared block's name (per Windows session).
const NAME: &str = "Local\\Snag.Preview";
pub const MAX_W: u32 = 960;
pub const MAX_H: u32 = 600;
const HEADER: usize = 16;
const SIZE: usize = HEADER + (MAX_W * MAX_H * 4) as usize;

/// Set by the controller while the window shows the preview.
pub static WANTED: AtomicBool = AtomicBool::new(false);
/// Previews a second the window asks for: 30 (Normal) or 60 (Smooth).
pub static RATE: AtomicU32 = AtomicU32::new(30);
/// How wide the window shows the preview, in pixels (0: not known yet).
/// Frames are shrunk to about that size instead of always 960 px wide.
pub static SHOWN_W: AtomicU32 = AtomicU32::new(0);

/// Recorder ticks between two previews at `fps` (one per tick at 60 fps
/// with Smooth, every other tick with Normal).
pub fn every(fps: u32) -> u64 {
    (fps as u64 / RATE.load(Ordering::Relaxed).max(1) as u64).max(1)
}

struct Mapping {
    handle: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
}

// SAFETY: the mapping is plain shared memory; access is synchronized by the
// seqlock protocol described above.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    fn ptr(&self) -> *mut u8 {
        self.view.Value as *mut u8
    }

    fn seq(&self) -> &AtomicU32 {
        // SAFETY: the view is at least HEADER bytes and 4-byte aligned.
        unsafe { &*(self.ptr() as *const AtomicU32) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: a view and handle we created.
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.handle);
        }
    }
}

// ---- core side ---------------------------------------------------------------

static SENDER: OnceLock<SyncSender<Arc<Nv12Frame>>> = OnceLock::new();

/// Offers a recorded frame for the preview. Never blocks: if the preview
/// thread is still busy with the previous one, this frame is skipped
/// (returns false; offer it again later).
pub fn offer(frame: &Arc<Nv12Frame>) -> bool {
    let tx = SENDER.get_or_init(|| {
        let (tx, rx) = sync_channel::<Arc<Nv12Frame>>(1);
        std::thread::Builder::new()
            .name("preview".into())
            .spawn(move || {
                let Some(map) = create(NAME) else { return };
                let mut rgba = Vec::new();
                for f in rx {
                    let (w, h) = shrink_to_rgba(&f, &mut rgba, SHOWN_W.load(Ordering::Relaxed));
                    drop(f); // give the buffer back to the recorder quickly
                    write(&map, w, h, &rgba);
                }
            })
            .expect("spawn preview thread");
        tx
    });
    tx.try_send(frame.clone()).is_ok()
}

fn create(name: &str) -> Option<Mapping> {
    // SAFETY: a pagefile-backed named mapping and a view of all of it.
    unsafe {
        let handle = CreateFileMappingW(INVALID_HANDLE_VALUE, None, PAGE_READWRITE, 0, SIZE as u32, &HSTRING::from(name)).ok()?;
        let view = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, SIZE);
        if view.Value.is_null() {
            let _ = CloseHandle(handle);
            log::warn!("preview: could not map shared memory");
            return None;
        }
        Some(Mapping { handle, view })
    }
}

fn write(map: &Mapping, w: u32, h: u32, rgba: &[u8]) {
    // The copy below must stay inside the shared block.
    if w > MAX_W || h > MAX_H || rgba.len() > SIZE - HEADER || rgba.len() < (w * h * 4) as usize {
        debug_assert!(false, "preview frame {w}x{h} ({} bytes) doesn't fit", rgba.len());
        return;
    }
    let seq = map.seq();
    let s = seq.load(Ordering::Relaxed);
    seq.store(s | 1, Ordering::Release); // odd: writing
    std::sync::atomic::fence(Ordering::Release);
    // SAFETY: header + pixels fit in SIZE (w <= MAX_W, h <= MAX_H).
    unsafe {
        let p = map.ptr();
        std::ptr::write_unaligned(p.add(4) as *mut u32, w);
        std::ptr::write_unaligned(p.add(8) as *mut u32, h);
        std::ptr::copy_nonoverlapping(rgba.as_ptr(), p.add(HEADER), rgba.len());
    }
    std::sync::atomic::fence(Ordering::Release);
    seq.store((s | 1).wrapping_add(1), Ordering::Release); // even: done
}

/// The whole-number step a frame is shrunk by: enough to fit MAX_W x MAX_H,
/// and more while the preview is shown smaller (the smallest result that's
/// still at least `shown_w` wide; 0 = not known).
pub fn step_for(w: u32, h: u32, shown_w: u32) -> usize {
    let fit = w.div_ceil(MAX_W).max(h.div_ceil(MAX_H)).max(1);
    let shown = w.checked_div(shown_w).map_or(1, |s| s.clamp(1, 8));
    fit.max(shown) as usize
}

/// Shrinks an NV12 frame (BT.709 limited range, what the GPU converter
/// produces) to RGBA no wider than MAX_W (and about `shown_w`), by a
/// whole-number step. Each output pixel averages a 2x2 block of brightness
/// samples (smooth edges, no shimmer), and the color math is integer-only:
/// about 1.5 ms for 1080p -> 960x540. Only new frames are shrunk (see the
/// pacer), so a static screen costs nothing.
pub fn shrink_to_rgba(f: &Nv12Frame, out: &mut Vec<u8>, shown_w: u32) -> (u32, u32) {
    let step = step_for(f.width, f.height, shown_w);
    if step == 2 {
        halve_to_rgba(f, out)
    } else {
        shrink_any(f, step, out)
    }
}

/// BT.709 limited-range YUV -> RGB, integer math (the color step of
/// `shrink_to_rgba`).
#[inline(always)]
fn yuv_to_rgb(yy: i32, u: u8, v: u8, o: &mut [u8]) {
    let c = 298 * (yy - 16);
    let (du, dv) = (u as i32 - 128, v as i32 - 128);
    let px = |v: i32| ((v + 128) >> 8).clamp(0, 255) as u8;
    o[0] = px(c + 459 * dv);
    o[1] = px(c - 55 * du - 136 * dv);
    o[2] = px(c + 541 * du);
}

/// `shrink_to_rgba` for the common case (1080p and similar: exactly half
/// size): the same math, walking the rows in pairs of pixels so there are no
/// per-pixel bounds checks (1.5 instead of 2 ms for 1080p here).
fn halve_to_rgba(f: &Nv12Frame, out: &mut Vec<u8>) -> (u32, u32) {
    let (fw, fh) = (f.width as usize, f.height as usize);
    let (w, h) = ((fw / 2) & !1, (fh / 2) & !1);
    out.clear();
    out.resize(w * h * 4, 255);
    let (luma, uv_plane) = f.data.split_at(fw * fh);
    for (y, dst) in out.chunks_exact_mut(w * 4).enumerate() {
        let sy = y * 2;
        let r0 = &luma[sy * fw..][..w * 2];
        let r1 = &luma[(sy + 1).min(fh - 1) * fw..][..w * 2];
        let uv = &uv_plane[(sy / 2) * fw..][..w * 2];
        let pairs = r0.as_chunks::<2>().0.iter().zip(r1.as_chunks::<2>().0).zip(uv.as_chunks::<2>().0);
        for (o, ((a, b), c)) in dst.as_chunks_mut::<4>().0.iter_mut().zip(pairs) {
            let yy = (a[0] as i32 + a[1] as i32 + b[0] as i32 + b[1] as i32 + 2) / 4;
            yuv_to_rgb(yy, c[0], c[1], o);
        }
    }
    (w as u32, h as u32)
}

/// Any whole-number step (bigger monitors, odd sizes).
fn shrink_any(f: &Nv12Frame, step: usize, out: &mut Vec<u8>) -> (u32, u32) {
    let (w, h) = ((f.width as usize / step) & !1, (f.height as usize / step) & !1);
    out.clear();
    out.resize(w * h * 4, 255);
    let (fw, fh) = (f.width as usize, f.height as usize);
    let (luma, uv_plane) = f.data.split_at(fw * fh);
    let d = (step / 2).max(1).min(step); // offset of the second sample
    for y in 0..h {
        let sy = y * step;
        let r0 = &luma[sy * fw..(sy + 1) * fw];
        let r1 = &luma[(sy + d).min(fh - 1) * fw..][..fw];
        let uv = &uv_plane[(sy / 2) * fw..][..fw];
        let dst = &mut out[y * w * 4..(y + 1) * w * 4];
        for x in 0..w {
            let sx = x * step;
            let sx2 = (sx + d).min(fw - 1);
            let yy = (r0[sx] as i32 + r0[sx2] as i32 + r1[sx] as i32 + r1[sx2] as i32 + 2) / 4;
            yuv_to_rgb(yy, uv[sx & !1], uv[(sx & !1) + 1], &mut dst[x * 4..x * 4 + 3]);
        }
    }
    (w as u32, h as u32)
}

// ---- window side -------------------------------------------------------------

/// Set by the window while it shows the preview; `watch` only looks then.
pub static WATCHING: AtomicBool = AtomicBool::new(false);
/// Set by the window for the Smooth (60 fps) preview: `watch` looks every
/// 4 ms instead of 15, so no frame waits for the next look.
pub static WATCH_FAST: AtomicBool = AtomicBool::new(false);

/// The core's block, read-only (None until the core created it).
fn open(name: &str) -> Option<Mapping> {
    // SAFETY: opening the core's named mapping read-only.
    unsafe {
        let handle = OpenFileMappingW(FILE_MAP_READ.0, false, &HSTRING::from(name)).ok()?;
        let view = MapViewOfFile(handle, FILE_MAP_READ, 0, 0, SIZE);
        if view.Value.is_null() {
            let _ = CloseHandle(handle);
            return None;
        }
        Some(Mapping { handle, view })
    }
}

/// Calls `on_new` (on its own thread) each time the core writes a new
/// preview frame, so the window redraws only then instead of 30 times a
/// second whether or not anything arrived (a static screen, the buffer off,
/// "Waiting for..."). Looks at the frame counter every 15 ms while
/// `WATCHING` is set, otherwise sleeps. Start it once; it lives as long as
/// the window process.
pub fn watch(on_new: impl Fn() + Send + 'static) {
    watch_named(NAME, on_new);
}

fn watch_named(name: &'static str, on_new: impl Fn() + Send + 'static) {
    let spawned = std::thread::Builder::new().name("preview-watch".into()).spawn(move || {
        let mut map: Option<Mapping> = None;
        let mut last = 0u32;
        loop {
            if !WATCHING.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            std::thread::sleep(Duration::from_millis(if WATCH_FAST.load(Ordering::Relaxed) { 4 } else { 15 }));
            let Some(m) = map.as_ref() else {
                map = open(name);
                if map.is_none() {
                    std::thread::sleep(Duration::from_millis(500)); // no core yet
                }
                continue;
            };
            let s = m.seq().load(Ordering::Acquire);
            if s & 1 == 0 && s != last {
                last = s;
                on_new();
            }
        }
    });
    if let Err(e) = spawned {
        log::warn!("preview watcher: {e}");
    }
}

/// The window's read-only view of the preview block.
#[derive(Default)]
pub struct Reader {
    map: Option<Mapping>,
    last_seq: u32,
}

impl Reader {
    /// A new frame, if the core wrote one since the last call: hands
    /// (width, height, rgba) to `make` straight from the shared block (the
    /// window turns them into its image with a single copy). What `make`
    /// returns is dropped if the core overwrote the frame meanwhile.
    pub fn next_frame_with<T>(&mut self, make: impl FnOnce(u32, u32, &[u8]) -> T) -> Option<T> {
        if self.map.is_none() {
            self.map = open(NAME);
        }
        let map = self.map.as_ref()?;
        let seq = map.seq();
        let before = seq.load(Ordering::Acquire);
        if before & 1 == 1 || before == self.last_seq {
            return None;
        }
        // SAFETY: header and pixels are within SIZE (sizes are clamped).
        // The writer is another process; if it rewrote the block while we
        // read (a torn frame), the sequence number below shows it and the
        // result is thrown away (the seqlock protocol).
        let (w, h, out) = unsafe {
            let p = map.ptr();
            let w = std::ptr::read_unaligned(p.add(4) as *const u32).min(MAX_W);
            let h = std::ptr::read_unaligned(p.add(8) as *const u32).min(MAX_H);
            let px = std::slice::from_raw_parts(p.add(HEADER), (w * h * 4) as usize);
            (w, h, make(w, h, px))
        };
        std::sync::atomic::fence(Ordering::Acquire);
        if seq.load(Ordering::Acquire) != before || w == 0 || h == 0 {
            return None; // overwritten while copying; take the next one
        }
        self.last_seq = before;
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_shrink_to_about_the_size_they_are_shown() {
        // Not known yet, or shown big: half size for 1080p (fits 960x600).
        assert_eq!(step_for(1920, 1080, 0), 2);
        assert_eq!(step_for(1920, 1080, 1300), 2);
        // A 730 px wide preview still gets 960 (the smallest that's wider).
        assert_eq!(step_for(1920, 1080, 730), 2);
        // Dragged small: a quarter (480 wide) is enough for 400 px.
        assert_eq!(step_for(1920, 1080, 400), 4);
        // 4K always needs at least a quarter; never more than an eighth.
        assert_eq!(step_for(3840, 2160, 0), 4);
        assert_eq!(step_for(3840, 2160, 100), 8);
        // Normal = 30 a second: every other frame at 60 fps.
        RATE.store(30, Ordering::Relaxed);
        assert_eq!((every(60), every(30)), (2, 1));
        RATE.store(60, Ordering::Relaxed);
        assert_eq!((every(60), every(30), every(144)), (1, 1, 2));
        RATE.store(30, Ordering::Relaxed);
    }

    #[test]
    fn shrinks_and_converts() {
        let mut f = Nv12Frame::new(1920, 1080);
        // Y = 235 (white) on the top half, 16 (black) below; neutral chroma.
        let (w, h) = (1920usize, 1080usize);
        for y in 0..h {
            for x in 0..w {
                f.data[y * w + x] = if y < h / 2 { 235 } else { 16 };
            }
        }
        for v in &mut f.data[w * h..] {
            *v = 128;
        }
        let mut out = Vec::new();
        assert_eq!(shrink_to_rgba(&f, &mut out, 0), (960, 540));
        assert_eq!(&out[..4], &[255, 255, 255, 255]);
        let last = out.len() - 4;
        assert_eq!(&out[last..], &[0, 0, 0, 255]);
    }

    #[test]
    fn fast_half_size_path_matches_the_general_one() {
        for (w, h) in [(1920u32, 1080u32), (1600, 900), (1280, 1024)] {
            let mut f = Nv12Frame::new(w, h);
            for (i, v) in f.data.iter_mut().enumerate() {
                *v = (i * 7 % 251) as u8;
            }
            let (mut fast, mut slow) = (Vec::new(), Vec::new());
            assert_eq!(halve_to_rgba(&f, &mut fast), shrink_any(&f, 2, &mut slow), "{w}x{h} size");
            assert!(fast == slow, "{w}x{h}: pixels differ");
        }
    }

    #[test]
    #[ignore = "timing only: cargo test --release --lib -- --ignored shrink_speed --nocapture"]
    fn shrink_speed() {
        let mut f = Nv12Frame::new(1920, 1080);
        for (i, v) in f.data.iter_mut().enumerate() {
            *v = (i * 7 % 251) as u8;
        }
        let mut out = Vec::new();
        let t = std::time::Instant::now();
        for _ in 0..200 {
            shrink_to_rgba(&f, &mut out, 0);
        }
        println!("shrink_to_rgba 1080p -> 960x540: {:.2} ms per frame", t.elapsed().as_secs_f64() * 1000.0 / 200.0);
    }

    #[test]
    fn watcher_wakes_the_window_once_per_new_frame() {
        // Its own block (never the real one, so a running Snag isn't touched).
        let name: &'static str = Box::leak(format!("Local\\Snag.Preview.test.{}", std::process::id()).into_boxed_str());
        let map = create(name).expect("test block");
        let wakes = Arc::new(AtomicU32::new(0));
        let w = wakes.clone();
        WATCHING.store(true, Ordering::Relaxed);
        watch_named(name, move || {
            w.fetch_add(1, Ordering::Relaxed);
        });
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(wakes.load(Ordering::Relaxed), 0, "no frame yet: no wake-up");
        let rgba = vec![200u8; 8 * 8 * 4];
        for n in 1..=3 {
            write(&map, 8, 8, &rgba);
            std::thread::sleep(Duration::from_millis(150));
            assert_eq!(wakes.load(Ordering::Relaxed), n, "one wake-up per new frame");
        }
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(wakes.load(Ordering::Relaxed), 3, "nothing new: no more wake-ups");
    }
}
