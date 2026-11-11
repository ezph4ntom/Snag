//! Clip verification for the reliability tests and `--selftest`.
//!
//! Test frames carry a 16-bit barcode in their top quarter: 16 vertical bars,
//! white = 1, black = 0, most significant bit on the left. After the full
//! pipeline (capture or synthetic source -> pacer -> encoder -> segments ->
//! clip), we decode every frame, read the barcode back and check:
//! * no black frames (mean luma),
//! * no duplicate frames (same barcode twice, or pixel-identical neighbours),
//! * no gaps/reordering (barcode must be previous + 1, or strictly
//!   increasing for live capture where the screen updates faster than we sample),
//! * the clip starts on a keyframe and timestamps are constant-frame-rate;
//! * if the clip has audio: its length matches the video, and (for the
//!   synthetic beep source) every beep lines up with the video frame taken
//!   at the same instant, which measures audio/video sync.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};

use crate::encode::Nv12Frame;

pub const BARCODE_BITS: u32 = 16;
/// Frames are decoded at this size for analysis (fast, plenty for 16 bars).
const AW: usize = 256;
const AH: usize = 144;

/// Draws synthetic frame number `counter` directly in NV12.
pub fn draw_test_frame_nv12(frame: &mut Nv12Frame, counter: u32) {
    let (w, h) = (frame.width as usize, frame.height as usize);
    let band = h / 4;
    let (luma, chroma) = frame.data.split_at_mut(w * h);
    let mut bar_row = vec![0u8; w];
    for (x, px) in bar_row.iter_mut().enumerate() {
        let bit = (x * BARCODE_BITS as usize / w) as u32;
        let on = (counter >> (BARCODE_BITS - 1 - bit)) & 1 == 1;
        *px = if on { 235 } else { 16 };
    }
    for y in 0..band {
        luma[y * w..(y + 1) * w].copy_from_slice(&bar_row);
    }
    // Below the barcode: a diagonal ramp that moves every frame, so the
    // encoder sees real motion (like a game), not a static image.
    let ramp: Vec<u8> = (0..w + 160).map(|i| 40 + (i % 160) as u8).collect();
    for y in band..h {
        let off = (y + counter as usize * 6) % 160;
        luma[y * w..(y + 1) * w].copy_from_slice(&ramp[off..off + w]);
    }
    chroma.fill(128); // neutral gray chroma
}

/// Reads the barcode from an `AW`x`AH` grayscale frame.
pub fn read_barcode(gray: &[u8]) -> Option<u32> {
    let row = AH / 8; // middle of the top-quarter band
    let mut value = 0u32;
    for bit in 0..BARCODE_BITS as usize {
        let cx = bit * AW / BARCODE_BITS as usize + AW / (2 * BARCODE_BITS as usize);
        let mut sum = 0u32;
        for dy in 0..3 {
            for dx in 0..3 {
                sum += gray[(row + dy - 1) * AW + cx + dx - 1] as u32;
            }
        }
        let avg = sum / 9;
        let b = match avg {
            0..=90 => 0,
            160..=255 => 1,
            _ => return None, // neither clearly white nor black: corrupt frame
        };
        value = (value << 1) | b;
    }
    Some(value)
}

#[derive(Debug, Default)]
pub struct ClipReport {
    pub path: PathBuf,
    pub frames: usize,
    pub fps: f64,
    pub first_is_keyframe: bool,
    /// Frame indices whose timestamp delta isn't exactly 1/fps.
    pub timing_errors: Vec<usize>,
    pub black_frames: Vec<usize>,
    pub duplicate_frames: Vec<usize>,
    pub unreadable_frames: Vec<usize>,
    /// `(frame index, previous code, this code)` where continuity broke.
    pub gaps: Vec<(usize, u32, u32)>,
    pub first_code: Option<u32>,
    /// Length of the audio track in seconds (None = no audio track).
    pub audio_seconds: Option<f64>,
    /// For each beep found in the audio: beep time minus the time of the
    /// video frame from the same instant, in milliseconds.
    pub av_offsets_ms: Vec<f64>,
}

impl ClipReport {
    pub fn is_ok(&self) -> bool {
        self.frames > 0
            && self.first_is_keyframe
            && self.timing_errors.is_empty()
            && self.black_frames.is_empty()
            && self.duplicate_frames.is_empty()
            && self.unreadable_frames.is_empty()
            && self.gaps.is_empty()
    }

    pub fn summary(&self) -> String {
        format!(
            "{}: {} frames @ {:.2} fps, keyframe start: {}, timing errors: {}, black: {}, duplicate: {}, unreadable: {}, gaps: {}{}",
            self.path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(),
            self.frames,
            self.fps,
            self.first_is_keyframe,
            self.timing_errors.len(),
            self.black_frames.len(),
            self.duplicate_frames.len(),
            self.unreadable_frames.len(),
            self.gaps.len(),
            self.gaps
                .first()
                .map(|g| format!(" (first gap at frame {}: {} -> {})", g.0, g.1, g.2))
                .unwrap_or_default()
        ) + &match self.audio_seconds {
            None => ", no audio".to_string(),
            Some(a) if self.av_offsets_ms.is_empty() => format!(", audio {a:.2} s"),
            Some(a) => format!(
                ", audio {a:.2} s, A/V sync {} beeps, worst {:+.1} ms",
                self.av_offsets_ms.len(),
                self.worst_av_offset_ms()
            ),
        }
    }

    /// The beep/frame offset with the largest magnitude (0 if none).
    pub fn worst_av_offset_ms(&self) -> f64 {
        self.av_offsets_ms.iter().copied().fold(0.0, |w, o| if o.abs() > w.abs() { o } else { w })
    }
}

/// Packet timestamps and keyframe flags via ffprobe (no decoding needed).
fn probe_packets(path: &Path) -> Result<(Vec<f64>, Vec<bool>, f64)> {
    let out = crate::win::hidden_command(crate::paths::ffprobe_exe())
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=r_frame_rate:packet=pts_time,flags", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .context("running ffprobe")?;
    if !out.status.success() {
        bail!("ffprobe failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut fps = 0.0;
    let mut pts = Vec::new();
    let mut key = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.trim().split(',').collect();
        match parts.as_slice() {
            [rate] if rate.contains('/') => {
                let mut it = rate.split('/');
                let n: f64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
                let d: f64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(1.0);
                fps = if d > 0.0 { n / d } else { 0.0 };
            }
            [t, flags, ..] => {
                if let Ok(t) = t.parse::<f64>() {
                    pts.push(t);
                    key.push(flags.contains('K'));
                }
            }
            _ => {}
        }
    }
    Ok((pts, key, fps))
}

/// A region of the clip to analyze, in clip pixels: `(x, y, w, h)`.
pub type Crop = (u32, u32, u32, u32);

/// Decodes all frames at `AW`x`AH` grayscale (optionally cropped first).
// (A 36 KB frame is better as a slice than as a fixed-size array chunk.)
#[allow(clippy::chunks_exact_to_as_chunks)]
fn decode_gray(path: &Path, crop: Option<Crop>) -> Result<Vec<Vec<u8>>> {
    let filter = match crop {
        Some((x, y, w, h)) => format!("crop={w}:{h}:{x}:{y},scale={AW}:{AH}:flags=area"),
        None => format!("scale={AW}:{AH}:flags=area"),
    };
    let mut child = crate::win::hidden_command(crate::paths::ffmpeg_exe())
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-an", "-vf", &filter, "-fps_mode", "passthrough"])
        .args(["-pix_fmt", "gray", "-f", "rawvideo", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("running ffmpeg to decode")?;
    let mut bytes = Vec::new();
    child.stdout.take().context("no stdout")?.read_to_end(&mut bytes)?;
    let status = child.wait()?;
    if !status.success() {
        bail!("decoding {} failed", path.display());
    }
    Ok(bytes.chunks_exact(AW * AH).map(<[u8]>::to_vec).collect())
}

/// Checks one clip. `consecutive`: barcodes must increase by exactly 1
/// (synthetic source); otherwise strictly increasing is enough (live capture).
pub fn verify_clip(path: &Path, consecutive: bool, crop: Option<Crop>) -> Result<ClipReport> {
    let (pts, key, fps) = probe_packets(path)?;
    let frames = decode_gray(path, crop)?;
    let mut r = ClipReport {
        path: path.to_path_buf(),
        frames: frames.len(),
        fps,
        first_is_keyframe: key.first().copied().unwrap_or(false),
        ..Default::default()
    };
    if frames.len() != pts.len() {
        bail!("decoded {} frames but the file has {} packets", frames.len(), pts.len());
    }
    let mut sorted = pts.clone();
    sorted.sort_by(f64::total_cmp);
    let expected = if fps > 0.0 { 1.0 / fps } else { 0.0 };
    for (i, pair) in sorted.windows(2).enumerate() {
        // MP4 timebases round; allow 1 ms of slack.
        if (pair[1] - pair[0] - expected).abs() > 0.001 {
            r.timing_errors.push(i + 1);
        }
    }

    let mut prev_code: Option<u32> = None;
    let mut prev_frame: Option<&Vec<u8>> = None;
    for (i, f) in frames.iter().enumerate() {
        let mean = f.iter().map(|&p| p as u64).sum::<u64>() as f64 / f.len() as f64;
        if mean < 20.0 {
            r.black_frames.push(i);
        }
        if let Some(p) = prev_frame {
            // Content-agnostic duplicate test: neighbours pixel-identical.
            let diff = f.iter().zip(p).map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as u64).sum::<u64>() as f64
                / f.len() as f64;
            if diff < 0.05 {
                r.duplicate_frames.push(i);
            }
        }
        match read_barcode(f) {
            None => r.unreadable_frames.push(i),
            Some(code) => {
                if r.first_code.is_none() {
                    r.first_code = Some(code);
                }
                if let Some(p) = prev_code {
                    if code == p {
                        if !r.duplicate_frames.contains(&i) {
                            r.duplicate_frames.push(i);
                        }
                    } else if (consecutive && code != (p + 1) & 0xFFFF) || (!consecutive && code < p) {
                        r.gaps.push((i, p, code));
                    }
                }
                prev_code = Some(code);
            }
        }
        prev_frame = Some(f);
    }

    // Audio: duration, and beep-vs-frame sync for synthetic recordings.
    if let Some((a_start, a_len, samples)) = decode_audio(path)? {
        r.audio_seconds = Some(a_len);
        let fps_i = fps.round().max(1.0) as u32;
        // Video frames whose barcode is a multiple of fps were taken at a
        // whole second of the recording, exactly when a beep starts.
        let markers: Vec<f64> = frames
            .iter()
            .enumerate()
            .filter_map(|(i, f)| read_barcode(f).filter(|c| c % fps_i == 0).map(|_| sorted[i]))
            .collect();
        for onset in beep_onsets(&samples) {
            let t_audio = a_start + onset as f64 / 48_000.0;
            if let Some(t_video) = markers.iter().copied().min_by(|a, b| (a - t_audio).abs().total_cmp(&(b - t_audio).abs())) {
                if (t_audio - t_video).abs() < 0.5 {
                    r.av_offsets_ms.push((t_audio - t_video) * 1000.0);
                }
            }
        }
    }
    Ok(r)
}

/// The audio track as mono 48 kHz samples: `(start time, duration, samples)`.
fn decode_audio(path: &Path) -> Result<Option<(f64, f64, Vec<f32>)>> {
    let out = crate::win::hidden_command(crate::paths::ffprobe_exe())
        .args(["-v", "error", "-select_streams", "a:0", "-show_entries", "stream=start_time", "-of", "csv=p=0"])
        .arg(path)
        .output()?;
    let start_text = String::from_utf8_lossy(&out.stdout);
    let Ok(start) = start_text.trim().trim_end_matches(',').parse::<f64>() else {
        return Ok(None); // no audio stream
    };
    let mut child = crate::win::hidden_command(crate::paths::ffmpeg_exe())
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-vn", "-ac", "1", "-ar", "48000", "-f", "f32le", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut bytes = Vec::new();
    child.stdout.take().context("no stdout")?.read_to_end(&mut bytes)?;
    child.wait()?;
    let samples: Vec<f32> = bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
    let len = samples.len() as f64 / 48_000.0;
    Ok(Some((start, len, samples)))
}

/// Sample indices where a beep starts: loud after at least 50 ms of quiet.
fn beep_onsets(samples: &[f32]) -> Vec<usize> {
    let mut onsets = Vec::new();
    let mut quiet = 0usize;
    for (i, &x) in samples.iter().enumerate() {
        if x.abs() > 0.05 {
            if quiet >= 2400 {
                onsets.push(i);
            }
            quiet = 0;
        } else {
            quiet += 1;
        }
    }
    onsets
}

/// A small always-on-top window that repaints a barcode of its own paint
/// counter every ~2 ms (faster than any refresh rate). Capturing the monitor it's on and
/// verifying the clip (cropped to the window) tests the real WGC path.
pub mod pattern {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::thread::JoinHandle;

    use anyhow::{Result, anyhow};
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::core::w;

    use super::BARCODE_BITS;

    pub struct PatternWindow {
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
        pub paints: Arc<AtomicU32>,
        /// Paint intervals longer than one 60 fps frame (16.7 ms). During such a
        /// gap the screen really didn't change, so a repeated frame is correct.
        pub long_gaps: Arc<AtomicU32>,
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        // SAFETY: default handling for everything.
        unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
    }

    fn paint(mem: HDC, w: i32, h: i32, counter: u32) {
        // SAFETY: GDI calls on a valid memory DC.
        unsafe {
            let white = CreateSolidBrush(COLORREF(0x00FF_FFFF));
            let black = CreateSolidBrush(COLORREF(0x0000_0000));
            let gray = CreateSolidBrush(COLORREF(0x0070_7070));
            let band = h / 4;
            for bit in 0..BARCODE_BITS as i32 {
                let on = (counter >> (BARCODE_BITS as i32 - 1 - bit)) & 1 == 1;
                let r = RECT { left: bit * w / 16, top: 0, right: (bit + 1) * w / 16, bottom: band };
                FillRect(mem, &r, if on { white } else { black });
            }
            let r = RECT { left: 0, top: band, right: w, bottom: h };
            FillRect(mem, &r, gray);
            // A block that moves every paint, like game motion.
            let x = ((counter as i32) * 9) % (w - 60).max(1);
            let r = RECT { left: x, top: band + 10, right: x + 60, bottom: h - 10 };
            FillRect(mem, &r, white);
            for b in [white, black, gray] {
                let _ = DeleteObject(b.into());
            }
        }
    }

    impl PatternWindow {
        /// Opens the window at `(x, y)` with size `w`x`h` (screen pixels).
        pub fn open(x: i32, y: i32, w: i32, h: i32) -> Result<PatternWindow> {
            let stop = Arc::new(AtomicBool::new(false));
            let paints = Arc::new(AtomicU32::new(0));
            let long_gaps = Arc::new(AtomicU32::new(0));
            let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
            let (s, p, g) = (stop.clone(), paints.clone(), long_gaps.clone());
            let thread = std::thread::spawn(move || {
                // SAFETY: standard window creation and a GDI render loop, all
                // on this thread.
                unsafe {
                    let hinst = GetModuleHandleW(None).unwrap_or_default();
                    let class = WNDCLASSW {
                        lpfnWndProc: Some(wndproc),
                        hInstance: hinst.into(),
                        lpszClassName: w!("SnagTestPattern"),
                        ..Default::default()
                    };
                    RegisterClassW(&class);
                    let hwnd = match CreateWindowExW(
                        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                        w!("SnagTestPattern"),
                        w!("Snag test pattern"),
                        WS_POPUP | WS_VISIBLE,
                        x, y, w, h,
                        None, None, Some(hinst.into()), None,
                    ) {
                        Ok(h) => h,
                        Err(e) => {
                            let _ = ready_tx.send(Err(e.to_string()));
                            return;
                        }
                    };
                    let dc = GetDC(Some(hwnd));
                    let mem = CreateCompatibleDC(Some(dc));
                    let bmp = CreateCompatibleBitmap(dc, w, h);
                    let old = SelectObject(mem, bmp.into());
                    let _ = ready_tx.send(Ok(()));
                    let mut msg = MSG::default();
                    let mut counter = 0u32;
                    let mut last_paint = std::time::Instant::now();
                    // Paint on time even while the test decodes clips on other cores.
                    let _ = windows::Win32::System::Threading::SetThreadPriority(
                        windows::Win32::System::Threading::GetCurrentThread(),
                        windows::Win32::System::Threading::THREAD_PRIORITY_HIGHEST,
                    );
                    while !s.load(Ordering::Relaxed) {
                        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                            let _ = TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                        // Draw offscreen, then copy in one blit so the
                        // compositor never sees a half-drawn barcode.
                        paint(mem, w, h, counter);
                        let _ = BitBlt(dc, 0, 0, w, h, Some(mem), 0, 0, SRCCOPY);
                        let _ = GdiFlush();
                        p.store(counter, Ordering::Relaxed);
                        if counter > 0 && last_paint.elapsed() > std::time::Duration::from_micros(16_667) {
                            g.fetch_add(1, Ordering::Relaxed);
                        }
                        last_paint = std::time::Instant::now();
                        counter = (counter + 1) & 0xFFFF;
                        // Repaint every ~2 ms, not once per vsync: pacing paints with
                        // DwmFlush misses compositions now and then (measured: ~5 gaps/s
                        // > 16.7 ms), during which the screen truly doesn't change and
                        // a repeated frame would be correct, not a pipeline bug.
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    SelectObject(mem, old);
                    let _ = DeleteObject(bmp.into());
                    let _ = DeleteDC(mem);
                    ReleaseDC(Some(hwnd), dc);
                    let _ = DestroyWindow(hwnd);
                }
            });
            ready_rx
                .recv()
                .map_err(|_| anyhow!("pattern thread died"))?
                .map_err(|e| anyhow!("pattern window: {e}"))?;
            Ok(PatternWindow { stop, thread: Some(thread), paints, long_gaps })
        }
    }

    impl Drop for PatternWindow {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }
}
