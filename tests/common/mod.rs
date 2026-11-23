//! Helpers shared by the "capture only this app" tests: small plain-colored
//! test windows (each on its own thread, never focused) and reading back the
//! color of a saved clip.
#![allow(dead_code)] // each test file uses a different part

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};
use std::time::{Duration, Instant};

use snag::buffer::Session;
use snag::buffer::clip::save_clip;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::CreateSolidBrush;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR};

pub const W: i32 = 480;
pub const H: i32 = 270;
pub const GREEN: u32 = 0x00C800;
pub const RED: u32 = 0xFF0000;

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: default handling for everything.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

/// Bottom-right corner of the primary monitor (Windows Graphics Capture
/// gets no frames from a window that is off every monitor).
pub fn corner() -> (i32, i32) {
    let mon = snag::capture::primary_monitor().expect("a monitor");
    (mon.x + mon.width as i32 - W - 80, mon.y + mon.height as i32 - H - 120)
}

pub enum Cmd {
    Minimize,
    Restore,
    Resize(i32, i32),
    /// SW_HIDE: still exists, just not shown (a game shutting down slowly).
    Hide,
    Show,
    /// Stop pumping messages for this long (the window "hangs").
    Hang(Duration),
    /// Stop pumping messages for this long while another thread keeps
    /// drawing (two greens, ~60 a second): a game busy loading, which
    /// Windows calls "Not responding" while it keeps rendering.
    HangDrawing(Duration),
    /// Repaint with a changing color about 90 times a second (like a game).
    Animate,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    /// A normal app window (in the taskbar), shown without focus.
    Normal,
    /// Always on top, not in the window list (a tool window).
    Cover,
    /// Created but never shown (a hidden helper window).
    Hidden,
    /// Shown, but a tool window (not an app window).
    Tool,
    /// Shown, owned by another window (a dialog).
    Owned(isize),
}

/// A plain colored window on its own thread; closed when dropped.
pub struct TestWindow {
    tx: Sender<Option<Cmd>>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub hwnd: isize,
}

impl TestWindow {
    pub fn open(title: &str, rgb: u32, x: i32, y: i32, kind: Kind) -> TestWindow {
        let (tx, rx) = channel::<Option<Cmd>>();
        let (ready_tx, ready_rx) = channel();
        let (title, class) = (HSTRING::from(title), HSTRING::from(format!("SnagWinTest{rgb:06x}")));
        let thread = std::thread::spawn(move || {
            // SAFETY: window creation and a message loop, all on this thread.
            unsafe {
                let hinst = GetModuleHandleW(None).unwrap_or_default();
                let bgr = ((rgb & 0xFF) << 16) | (rgb & 0xFF00) | (rgb >> 16);
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(wndproc),
                    hInstance: hinst.into(),
                    lpszClassName: PCWSTR(class.as_ptr()),
                    hbrBackground: CreateSolidBrush(COLORREF(bgr)),
                    ..Default::default()
                };
                RegisterClassW(&wc); // fails harmlessly if another window registered it
                let ex = match kind {
                    Kind::Cover => WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                    Kind::Tool => WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                    _ => WS_EX_NOACTIVATE,
                };
                let owner = match kind {
                    Kind::Owned(o) => Some(HWND(o as *mut _)),
                    _ => None,
                };
                let hwnd = CreateWindowExW(ex, PCWSTR(class.as_ptr()), PCWSTR(title.as_ptr()), WS_POPUP, x, y, W, H, owner, None, Some(hinst.into()), None)
                    .expect("test window");
                if kind != Kind::Hidden {
                    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                }
                let _ = ready_tx.send(hwnd.0 as isize);
                let mut msg = MSG::default();
                let mut animate: Option<u32> = None;
                loop {
                    if let Some(n) = animate.as_mut() {
                        *n = n.wrapping_add(1);
                        let level = (*n * 3 % 200) + 40;
                        let dc = windows::Win32::Graphics::Gdi::GetDC(Some(hwnd));
                        let brush = CreateSolidBrush(COLORREF(level << 8 | (255 - level)));
                        let mut r = windows::Win32::Foundation::RECT::default();
                        let _ = GetClientRect(hwnd, &mut r);
                        windows::Win32::Graphics::Gdi::FillRect(dc, &r, brush);
                        let _ = windows::Win32::Graphics::Gdi::DeleteObject(brush.into());
                        windows::Win32::Graphics::Gdi::ReleaseDC(Some(hwnd), dc);
                    }
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                    match rx.try_recv() {
                        Ok(Some(Cmd::Minimize)) => {
                            let _ = ShowWindow(hwnd, SW_SHOWMINNOACTIVE);
                        }
                        Ok(Some(Cmd::Restore | Cmd::Show)) => {
                            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                        }
                        Ok(Some(Cmd::Hide)) => {
                            let _ = ShowWindow(hwnd, SW_HIDE);
                        }
                        Ok(Some(Cmd::Resize(w, h))) => {
                            let _ = SetWindowPos(hwnd, None, 0, 0, w, h, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
                        }
                        Ok(Some(Cmd::Hang(d))) => std::thread::sleep(d), // no message pumping meanwhile
                        Ok(Some(Cmd::HangDrawing(d))) => {
                            let h = hwnd.0 as isize;
                            let painter = std::thread::spawn(move || {
                                let hwnd = HWND(h as *mut _);
                                let end = std::time::Instant::now() + d;
                                let mut n = 0u32;
                                while std::time::Instant::now() < end {
                                    n += 1;
                                    let g = if n.is_multiple_of(2) { 0xC8 } else { 0xB4 };
                                    // (Still inside the unsafe block above.) GDI drawing on
                                    // our own window from another thread: no messages
                                    // involved; the DC is released.
                                    let dc = windows::Win32::Graphics::Gdi::GetDC(Some(hwnd));
                                    let brush = CreateSolidBrush(COLORREF(g << 8));
                                    let mut r = windows::Win32::Foundation::RECT::default();
                                    let _ = GetClientRect(hwnd, &mut r);
                                    windows::Win32::Graphics::Gdi::FillRect(dc, &r, brush);
                                    let _ = windows::Win32::Graphics::Gdi::DeleteObject(brush.into());
                                    windows::Win32::Graphics::Gdi::ReleaseDC(Some(hwnd), dc);
                                    std::thread::sleep(Duration::from_millis(16));
                                }
                            });
                            std::thread::sleep(d); // no message pumping meanwhile
                            let _ = painter.join();
                        }
                        Ok(Some(Cmd::Animate)) => animate = Some(0),
                        Ok(None) | Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                        Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                let _ = DestroyWindow(hwnd);
            }
        });
        let hwnd = ready_rx.recv().expect("window thread");
        TestWindow { tx, thread: Some(thread), hwnd }
    }

    pub fn send(&self, c: Cmd) {
        let _ = self.tx.send(Some(c));
    }
}

impl Drop for TestWindow {
    fn drop(&mut self) {
        let _ = self.tx.send(None);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Average RGB of the last frame of a clip (scaled to 64x36).
pub fn last_frame_color(clip: &Path) -> [f64; 3] {
    let out = snag::win::hidden_command(snag::paths::ffmpeg_exe())
        .args(["-v", "error", "-nostdin", "-sseof", "-0.3", "-i"])
        .arg(clip)
        .args(["-frames:v", "1", "-vf", "scale=64:36", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .output()
        .expect("ffmpeg");
    assert_eq!(out.stdout.len(), 64 * 36 * 3, "decoding {} failed", clip.display());
    let mut sum = [0f64; 3];
    for px in out.stdout.as_chunks::<3>().0 {
        for k in 0..3 {
            sum[k] += px[k] as f64;
        }
    }
    sum.map(|s| s / (64.0 * 36.0))
}

/// Saves the last 1.5 s as `dir/name.mp4`.
pub fn clip(session: &Arc<Session>, dir: &Path, name: &str) -> PathBuf {
    let r = save_clip(session, Instant::now(), 1.5, dir, None).expect("save clip");
    let to = dir.join(format!("{name}.mp4"));
    std::fs::rename(&r.path, &to).unwrap();
    to
}

/// Waits up to `secs` for `ok`; panics with `what` if it never happens.
pub fn wait_for(what: &str, secs: u64, ok: impl FnMut() -> bool) {
    assert!(wait_until(Duration::from_secs(secs), ok).is_some(), "timed out waiting for {what}");
}

/// How long until `ok` became true, or None if it didn't within `limit`.
pub fn wait_until(limit: Duration, mut ok: impl FnMut() -> bool) -> Option<Duration> {
    let start = Instant::now();
    while !ok() {
        if start.elapsed() > limit {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Some(start.elapsed())
}

pub fn is_green(c: [f64; 3]) -> bool {
    c[1] > 120.0 && c[0] < 60.0 && c[2] < 60.0
}

pub fn is_black(c: [f64; 3]) -> bool {
    c.iter().all(|v| *v < 25.0)
}
