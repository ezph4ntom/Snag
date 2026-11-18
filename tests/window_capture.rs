//! "Capture only this app": records a test window while another window
//! covers it, then closes, minimizes and reopens it.
//!
//! Two small windows (green target, red cover on top) show in the corner of
//! the primary monitor for about 20 s. They never take focus. Run with:
//!
//! ```text
//! cargo test --release --test window_capture -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Sender, channel};
use std::time::{Duration, Instant};

use snag::buffer::clip::save_clip;
use snag::buffer::source::{FrameSource, Source, WgcSource, WindowTarget};
use snag::buffer::{Recorder, RecorderConfig, Session, SourceFactory};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::CreateSolidBrush;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR};

const TITLE: &str = "Snag window-capture test target";
const W: i32 = 480;
const H: i32 = 270;

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: default handling for everything.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

enum Cmd {
    Minimize,
    Restore,
    Resize(i32, i32),
}

/// A plain colored window on its own thread; closed when dropped.
struct TestWindow {
    tx: Sender<Option<Cmd>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TestWindow {
    /// `cover`: always on top and not in the window list (a tool window).
    fn open(title: &str, rgb: u32, x: i32, y: i32, cover: bool) -> TestWindow {
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
                RegisterClassW(&wc);
                let ex = if cover { WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE } else { WS_EX_NOACTIVATE };
                let hwnd = CreateWindowExW(ex, PCWSTR(class.as_ptr()), PCWSTR(title.as_ptr()), WS_POPUP, x, y, W, H, None, None, Some(hinst.into()), None)
                    .expect("test window");
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                let _ = ready_tx.send(());
                let mut msg = MSG::default();
                loop {
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                    match rx.try_recv() {
                        Ok(Some(Cmd::Minimize)) => {
                            let _ = ShowWindow(hwnd, SW_SHOWMINNOACTIVE);
                        }
                        Ok(Some(Cmd::Restore)) => {
                            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                        }
                        Ok(Some(Cmd::Resize(w, h))) => {
                            let _ = SetWindowPos(hwnd, None, 0, 0, w, h, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
                        }
                        Ok(None) | Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                        Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                let _ = DestroyWindow(hwnd);
            }
        });
        ready_rx.recv().expect("window thread");
        TestWindow { tx, thread: Some(thread) }
    }

    fn send(&self, c: Cmd) {
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
fn last_frame_color(clip: &Path) -> [f64; 3] {
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

fn clip(session: &std::sync::Arc<Session>, dir: &Path, name: &str) -> PathBuf {
    let r = save_clip(session, Instant::now(), 1.5, dir, None).expect("save clip");
    let to = dir.join(format!("{name}.mp4"));
    std::fs::rename(&r.path, &to).unwrap();
    to
}

fn wait_for(what: &str, secs: u64, ok: impl Fn() -> bool) {
    let until = Instant::now() + Duration::from_secs(secs);
    while !ok() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
#[ignore = "shows two small windows for ~20 s, needs a desktop session; run with --ignored"]
fn records_only_the_chosen_window() {
    snag::logging::init("test-window-capture");
    snag::encode::detect::ffmpeg_version().expect("ffmpeg must be installed (see README)");
    let probes = snag::encode::detect::probe_all();
    let enc = probes.iter().find(|p| p.works).expect("a working encoder");
    let root = std::env::temp_dir().join(format!("snag-window-capture-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let clips = root.join("clips");
    std::fs::create_dir_all(&clips).unwrap();

    let mon = snag::capture::primary_monitor().expect("a monitor");
    let (x, y) = (mon.x + mon.width as i32 - W - 80, mon.y + mon.height as i32 - H - 120);
    let exe = std::env::current_exe().unwrap().file_name().unwrap().to_string_lossy().into_owned();
    let green = 0x00C800;
    let mut target = Some(TestWindow::open(TITLE, green, x, y, false));
    let _cover = TestWindow::open("cover", 0xFF0000, x, y, true);
    std::thread::sleep(Duration::from_millis(500));
    assert!(snag::capture::window::find(&exe, TITLE).is_some(), "the target window is listed");

    let t = WindowTarget { exe: exe.clone(), title: TITLE.into() };
    let source: SourceFactory = Box::new(move || Ok(Box::new(WgcSource::new(Source::Window(t), 1280, 720, false)?) as Box<dyn FrameSource>));
    let cfg = RecorderConfig {
        encoder: enc.kind,
        width: 1280,
        height: 720,
        fps: 30,
        bitrate_kbps: 6000,
        buffer_seconds: 10,
        segment_seconds: 1,
        session_dir: root.join("buffer"),
        gpu_upload: enc.gpu_upload,
        audio_bitrate_kbps: 128,
    };
    let rec = Recorder::start(cfg, source, None).expect("recorder");
    let s = rec.session.clone();
    wait_for("recording to start", 10, || s.start_instant().is_some());
    std::thread::sleep(Duration::from_secs(3));

    let is_green = |c: [f64; 3]| c[1] > 120.0 && c[0] < 60.0 && c[2] < 60.0;
    let is_black = |c: [f64; 3]| c.iter().all(|v| *v < 25.0);

    // 1. Covered by a red window: the clip still shows the green target.
    assert!(!s.waiting.load(Ordering::Relaxed));
    let c1 = last_frame_color(&clip(&s, &clips, "1-covered"));
    println!("covered: {c1:?}");
    assert!(is_green(c1), "covered window: expected green, got {c1:?}");

    // 2. Minimized: black, and the session says it's waiting.
    target.as_ref().unwrap().send(Cmd::Minimize);
    wait_for("waiting (minimized)", 5, || s.waiting.load(Ordering::Relaxed));
    std::thread::sleep(Duration::from_secs(2));
    let c2 = last_frame_color(&clip(&s, &clips, "2-minimized"));
    println!("minimized: {c2:?}");
    assert!(is_black(c2), "minimized: expected black, got {c2:?}");

    // 3. Restored: green again.
    target.as_ref().unwrap().send(Cmd::Restore);
    wait_for("capturing again (restored)", 5, || !s.waiting.load(Ordering::Relaxed));
    std::thread::sleep(Duration::from_secs(2));
    let c3 = last_frame_color(&clip(&s, &clips, "3-restored"));
    println!("restored: {c3:?}");
    assert!(is_green(c3), "restored: expected green, got {c3:?}");

    // 3b. Resized to a square: letterboxed into the same 1280x720 clip
    // (green in the middle, black bars), nothing restarts.
    target.as_ref().unwrap().send(Cmd::Resize(H, H));
    std::thread::sleep(Duration::from_secs(2));
    let c3b = last_frame_color(&clip(&s, &clips, "3b-resized"));
    println!("resized: {c3b:?}");
    let bars = c3b[1] > 70.0 && c3b[1] < 150.0 && c3b[0] < 40.0 && c3b[2] < 40.0;
    assert!(bars, "resized: expected green with black bars, got {c3b:?}");
    target.as_ref().unwrap().send(Cmd::Resize(W, H));

    // 4. Closed: black and waiting.
    drop(target.take());
    wait_for("waiting (closed)", 5, || s.waiting.load(Ordering::Relaxed));
    std::thread::sleep(Duration::from_secs(2));
    let c4 = last_frame_color(&clip(&s, &clips, "4-closed"));
    println!("closed: {c4:?}");
    assert!(is_black(c4), "closed: expected black, got {c4:?}");

    // 5. Reopened: found again by exe name (looked for every 2 s).
    target = Some(TestWindow::open(TITLE, green, x, y, false));
    wait_for("re-attach (reopened)", 8, || !s.waiting.load(Ordering::Relaxed));
    std::thread::sleep(Duration::from_secs(2));
    let c5 = last_frame_color(&clip(&s, &clips, "5-reopened"));
    println!("reopened: {c5:?}");
    assert!(is_green(c5), "reopened: expected green, got {c5:?}");

    assert!(s.is_running(), "recorder error: {:?}", s.error());
    rec.stop();
    drop(target);
    println!("clips in {}", clips.display());
}
