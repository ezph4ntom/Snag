//! Live preview: the core hands small RGBA copies of the frames it records
//! to the window process through shared memory.
//!
//! Only while the window asks for it (Live tab visible): the recorder then
//! offers every other frame (about 30 per second) to a preview thread, which
//! shrinks the encoder's NV12 frame to at most 960 pixels wide, converts it
//! to RGBA and writes it into a named shared-memory block. The window maps
//! the same block and redraws when the frame counter changes.
//!
//! Layout of the block: `[seq u32][width u32][height u32][pad u32][rgba...]`.
//! `seq` is odd while a frame is being written (a "seqlock"): the reader
//! copies only when it reads the same even `seq` before and after copying.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, OnceLock};

use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile,
};
use windows::core::w;

use crate::encode::Nv12Frame;

pub const MAX_W: u32 = 960;
pub const MAX_H: u32 = 600;
const HEADER: usize = 16;
const SIZE: usize = HEADER + (MAX_W * MAX_H * 4) as usize;

/// Set by the controller while the window shows the preview.
pub static WANTED: AtomicBool = AtomicBool::new(false);

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
/// thread is still busy with the previous one, this frame is skipped.
pub fn offer(frame: &Arc<Nv12Frame>) {
    let tx = SENDER.get_or_init(|| {
        let (tx, rx) = sync_channel::<Arc<Nv12Frame>>(1);
        std::thread::Builder::new()
            .name("preview".into())
            .spawn(move || {
                let Some(map) = create() else { return };
                let mut rgba = Vec::new();
                for f in rx {
                    let (w, h) = shrink_to_rgba(&f, &mut rgba);
                    drop(f); // give the buffer back to the recorder quickly
                    write(&map, w, h, &rgba);
                }
            })
            .expect("spawn preview thread");
        tx
    });
    let _ = tx.try_send(frame.clone());
}

fn create() -> Option<Mapping> {
    // SAFETY: a pagefile-backed named mapping and a view of all of it.
    unsafe {
        let handle =
            CreateFileMappingW(INVALID_HANDLE_VALUE, None, PAGE_READWRITE, 0, SIZE as u32, w!("Local\\Snag.Preview")).ok()?;
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

/// Shrinks an NV12 frame (BT.709 limited range, what the GPU converter
/// produces) to RGBA no wider than MAX_W, by a whole-number step. Each output
/// pixel averages a 2x2 block of brightness samples (smooth edges, no
/// shimmer), and the color math is integer-only: about 2 ms for 1080p ->
/// 960x540, so 30 previews a second cost a few percent of one core.
pub fn shrink_to_rgba(f: &Nv12Frame, out: &mut Vec<u8>) -> (u32, u32) {
    let step = f.width.div_ceil(MAX_W).max(f.height.div_ceil(MAX_H)).max(1) as usize;
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
            let c = 298 * (yy - 16);
            let du = uv[sx & !1] as i32 - 128;
            let dv = uv[(sx & !1) + 1] as i32 - 128;
            let px = |v: i32| ((v + 128) >> 8).clamp(0, 255) as u8;
            let o = &mut dst[x * 4..x * 4 + 3];
            o[0] = px(c + 459 * dv);
            o[1] = px(c - 55 * du - 136 * dv);
            o[2] = px(c + 541 * du);
        }
    }
    (w as u32, h as u32)
}

// ---- window side -------------------------------------------------------------

/// The window's read-only view of the preview block.
#[derive(Default)]
pub struct Reader {
    map: Option<Mapping>,
    last_seq: u32,
}


impl Reader {
    /// A new frame, if the core wrote one since the last call:
    /// (width, height, rgba).
    pub fn next_frame(&mut self) -> Option<(u32, u32, Vec<u8>)> {
        if self.map.is_none() {
            // SAFETY: opening the core's named mapping read-only.
            self.map = unsafe {
                let handle = OpenFileMappingW(FILE_MAP_READ.0, false, w!("Local\\Snag.Preview")).ok()?;
                let view = MapViewOfFile(handle, FILE_MAP_READ, 0, 0, SIZE);
                if view.Value.is_null() {
                    let _ = CloseHandle(handle);
                    return None;
                }
                Some(Mapping { handle, view })
            };
        }
        let map = self.map.as_ref()?;
        let seq = map.seq();
        let before = seq.load(Ordering::Acquire);
        if before & 1 == 1 || before == self.last_seq {
            return None;
        }
        // SAFETY: header and pixels are within SIZE; sizes are clamped.
        let (w, h, px) = unsafe {
            let p = map.ptr();
            let w = std::ptr::read_unaligned(p.add(4) as *const u32).min(MAX_W);
            let h = std::ptr::read_unaligned(p.add(8) as *const u32).min(MAX_H);
            let mut px = vec![0u8; (w * h * 4) as usize];
            std::ptr::copy_nonoverlapping(p.add(HEADER), px.as_mut_ptr(), px.len());
            (w, h, px)
        };
        std::sync::atomic::fence(Ordering::Acquire);
        if seq.load(Ordering::Acquire) != before || w == 0 || h == 0 {
            return None; // overwritten while copying; take the next one
        }
        self.last_seq = before;
        Some((w, h, px))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(shrink_to_rgba(&f, &mut out), (960, 540));
        assert_eq!(&out[..4], &[255, 255, 255, 255]);
        let last = out.len() - 4;
        assert_eq!(&out[last..], &[0, 0, 0, 255]);
    }
}
