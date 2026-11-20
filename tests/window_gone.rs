//! "Capture only this app" when the app's window goes away in unusual ways
//! (a friend quit FiveM from its console and the clip froze on the last game
//! frame instead of going black). Each case records a green test window,
//! makes it go away, and checks that Snag says "waiting" and records black
//! within 3 s, then that it records the window again when it's back:
//!
//! * a. hidden (SW_HIDE) but not destroyed, like a game shutting down slowly
//! * b. hung: stops pumping messages (Windows calls it "Not responding"
//!   after 5 s; the 3 s count from then)
//! * c. destroyed while its process keeps running
//! * d. its process is killed / exits (a child process owns the window)
//! * e. destroyed while the app still has a hidden helper, a tool window
//!   and a dialog: Snag must not attach to those
//!
//! Small test windows show in the corner of the primary monitor (never
//! focused) for about 90 s in total. Run with:
//!
//! ```text
//! cargo test --release --test window_gone -- --ignored --nocapture --test-threads=1
//! ```

mod common;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::*;
use snag::buffer::source::{FrameSource, Source, WgcSource, WindowTarget};
use snag::buffer::{Recorder, RecorderConfig, Session, SourceFactory};

const TITLE: &str = "Snag window-gone test target";
/// Set for the child process of case d (see `window_host`).
const HOST_ENV: &str = "SNAG_TEST_WINDOW_HOST";
/// "exit": the child exits by itself after 15 s instead of waiting to be killed.
const HOST_EXIT_ENV: &str = "SNAG_TEST_WINDOW_HOST_EXIT";
/// How soon after the window goes away the clip must be black.
const GONE_LIMIT: Duration = Duration::from_secs(3);
/// Windows marks a window "Not responding" after 5 s without messages.
const HUNG_AFTER: Duration = Duration::from_secs(5);

/// The window of cases b and d, in a separate process like a real game (the
/// same test exe, so Snag finds it by the same program name). A line
/// "hang N" on stdin makes it stop responding for N seconds. Does nothing
/// in a normal test run.
#[test]
#[ignore = "helper for window_going_away_records_black (does nothing on its own)"]
fn window_host() {
    if std::env::var_os(HOST_ENV).is_none() {
        return;
    }
    let (x, y) = corner();
    let w = TestWindow::open(TITLE, GREEN, x, y, Kind::Normal);
    if std::env::var_os(HOST_EXIT_ENV).is_some() {
        std::thread::sleep(Duration::from_secs(15));
        std::process::exit(0); // the window is never closed by us: Windows cleans up
    }
    for line in std::io::stdin().lines().map_while(Result::ok) {
        if let Some(secs) = line.strip_prefix("hang ").and_then(|s| s.trim().parse().ok()) {
            w.send(Cmd::Hang(Duration::from_secs(secs)));
        }
    }
    std::thread::sleep(Duration::from_secs(120)); // killed long before
}

/// Kills the child window process if the test ends early.
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_host(exit_by_itself: bool) -> ChildGuard {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--ignored", "--exact", "window_host", "--nocapture", "--test-threads=1"])
        .env(HOST_ENV, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if exit_by_itself {
        cmd.env(HOST_EXIT_ENV, "1");
    }
    ChildGuard(cmd.spawn().expect("spawn the window host"))
}

struct Outcome {
    case: &'static str,
    /// How long after going away Snag said "waiting" (None: not within the limit).
    noticed: Option<Duration>,
    clip_black: bool,
    came_back: bool,
}

impl Outcome {
    fn ok(&self) -> bool {
        self.noticed.is_some() && self.clip_black && self.came_back
    }
}

struct Ctx {
    s: Arc<Session>,
    clips: std::path::PathBuf,
}

impl Ctx {
    fn waiting(&self) -> bool {
        self.s.waiting.load(Ordering::Relaxed)
    }

    /// Snag records the green window (not waiting, clip green).
    fn recording_green(&self, name: &str) -> bool {
        if wait_until(Duration::from_secs(8), || !self.waiting()).is_none() {
            println!("  {name}: still waiting after 8 s");
            return false;
        }
        std::thread::sleep(Duration::from_secs(2));
        let c = last_frame_color(&clip(&self.s, &self.clips, name));
        println!("  {name}: {c:?}");
        is_green(c)
    }

    /// After the window went away (at `since`): waiting within `limit`, and
    /// the clip is black.
    fn gone(&self, name: &str, since: Instant, limit: Duration) -> (Option<Duration>, bool) {
        let left = limit.saturating_sub(since.elapsed());
        let noticed = wait_until(left, || self.waiting()).map(|_| since.elapsed());
        std::thread::sleep(Duration::from_secs(2));
        let c = last_frame_color(&clip(&self.s, &self.clips, name));
        println!("  {name}: noticed {noticed:?}, last frame {c:?}");
        (noticed, is_black(c))
    }

    /// Waits until nothing of ours is captured (between cases).
    fn settle(&self) {
        let _ = wait_until(Duration::from_secs(5), || self.waiting());
    }
}

fn start(root: &Path) -> (Recorder, Ctx) {
    snag::encode::detect::ffmpeg_version().expect("ffmpeg must be installed (see README)");
    let probes = snag::encode::detect::probe_all();
    let enc = probes.iter().find(|p| p.works).expect("a working encoder");
    let clips = root.join("clips");
    std::fs::create_dir_all(&clips).unwrap();
    let exe = std::env::current_exe().unwrap().file_name().unwrap().to_string_lossy().into_owned();
    let t = WindowTarget { exe, title: TITLE.into() };
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
    (rec, Ctx { s, clips })
}

#[test]
#[ignore = "shows small windows for ~90 s, needs a desktop session; run with --ignored"]
fn window_going_away_records_black() {
    snag::logging::init("test-window-gone");
    let root = std::env::temp_dir().join(format!("snag-window-gone-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (rec, cx) = start(&root);
    let (x, y) = corner();
    let mut out = Vec::new();

    // a. Hidden but not destroyed.
    println!("case a: hidden");
    let w = TestWindow::open(TITLE, GREEN, x, y, Kind::Normal);
    assert!(cx.recording_green("a0-visible"), "case a: the window isn't recorded to begin with");
    w.send(Cmd::Hide);
    let (noticed, clip_black) = cx.gone("a1-hidden", Instant::now(), GONE_LIMIT);
    w.send(Cmd::Show);
    let came_back = cx.recording_green("a2-shown-again");
    out.push(Outcome { case: "a. hidden (SW_HIDE), not destroyed", noticed, clip_black, came_back });
    drop(w);
    cx.settle();

    // b. Hung (stops pumping messages for 14 s). In a child process: a real
    // game is another process (reading the title of a hung window of our
    // own process would wait for it).
    println!("case b: hung");
    let mut host = spawn_host(false);
    assert!(cx.recording_green("b0-visible"), "case b: the child's window isn't recorded to begin with");
    let exe = std::env::current_exe().unwrap().file_name().unwrap().to_string_lossy().into_owned();
    let target = snag::capture::window::find(&exe, TITLE).expect("the child's window");
    let hang = Duration::from_secs(14);
    let hung_at = Instant::now();
    {
        use std::io::Write;
        let stdin = host.0.stdin.as_mut().expect("child stdin");
        writeln!(stdin, "hang {}", hang.as_secs()).unwrap();
        stdin.flush().unwrap();
    }
    // Windows only calls a window "not responding" while messages wait for
    // it unanswered. A real game always has some (input, the shell asking
    // whether it's alive); here: a mouse move and a ping, like the shell's.
    let poker = std::thread::spawn(move || {
        use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::*;
        while hung_at.elapsed() < hang {
            let h = HWND(target as *mut _);
            // SAFETY: a mouse-move message and a 50 ms ping to the test's
            // own child window; a stale handle just fails.
            unsafe {
                let _ = PostMessageW(Some(h), WM_MOUSEMOVE, WPARAM(0), LPARAM(0));
                let mut r = 0usize;
                let _ = SendMessageTimeoutW(h, WM_NULL, WPARAM(0), LPARAM(0), SMTO_ABORTIFHUNG | SMTO_BLOCK, 50, Some(&mut r));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    });
    let (noticed, clip_black) = cx.gone("b1-hung", hung_at, HUNG_AFTER + GONE_LIMIT);
    let _ = poker.join();
    std::thread::sleep(hang.saturating_sub(hung_at.elapsed()));
    let came_back = cx.recording_green("b2-responding-again");
    out.push(Outcome { case: "b. hung (not responding), process alive", noticed, clip_black, came_back });
    drop(host);
    cx.settle();

    // c. Destroyed while the process keeps running.
    println!("case c: destroyed, process alive");
    let w = TestWindow::open(TITLE, GREEN, x, y, Kind::Normal);
    assert!(cx.recording_green("c0-visible"), "case c: the window isn't recorded to begin with");
    drop(w);
    let (noticed, clip_black) = cx.gone("c1-destroyed", Instant::now(), GONE_LIMIT);
    let w = TestWindow::open(TITLE, GREEN, x, y, Kind::Normal);
    let came_back = cx.recording_green("c2-reopened");
    out.push(Outcome { case: "c. destroyed, process alive", noticed, clip_black, came_back });
    drop(w);
    cx.settle();

    // e. Destroyed, but the app still has a hidden helper, a tool window and
    // a dialog (all with the same title): none of them is the app's window.
    println!("case e: leftovers of the same app");
    let w = TestWindow::open(TITLE, GREEN, x, y, Kind::Normal);
    assert!(cx.recording_green("e0-visible"), "case e: the window isn't recorded to begin with");
    let helper = TestWindow::open(TITLE, GREEN, x, y, Kind::Hidden);
    let tool = TestWindow::open(TITLE, GREEN, x, y, Kind::Tool);
    let dialog = TestWindow::open(TITLE, GREEN, x, y, Kind::Owned(helper.hwnd));
    drop(w);
    let (noticed, clip_black) = cx.gone("e1-destroyed", Instant::now(), GONE_LIMIT);
    // It must stay black: give Snag a few looks (every 2 s) at the leftovers.
    std::thread::sleep(Duration::from_secs(5));
    let stays = cx.waiting();
    let c = last_frame_color(&clip(&cx.s, &cx.clips, "e2-still-gone"));
    println!("  e2-still-gone: waiting {stays}, last frame {c:?}");
    let w = TestWindow::open(TITLE, GREEN, x, y, Kind::Normal);
    let came_back = cx.recording_green("e3-reopened");
    out.push(Outcome { case: "e. leftovers (hidden/tool/dialog) not attached", noticed, clip_black: clip_black && stays && is_black(c), came_back });
    drop((w, dialog, tool, helper));
    cx.settle();

    // d. The process that owns the window is killed, or exits by itself.
    for (exit_by_itself, case, name) in
        [(false, "d. process killed", "d1-killed"), (true, "d2. process exits by itself", "d2-exited")]
    {
        println!("case {case}");
        let mut host = spawn_host(exit_by_itself);
        assert!(cx.recording_green(&format!("{name}-before")), "{case}: the child's window isn't recorded");
        let since = if exit_by_itself {
            // The child exits about 15 s after it started.
            let exited = wait_until(Duration::from_secs(20), || host.0.try_wait().ok().flatten().is_some());
            assert!(exited.is_some(), "{case}: the child didn't exit");
            Instant::now()
        } else {
            let _ = host.0.kill();
            Instant::now()
        };
        let (noticed, clip_black) = cx.gone(name, since, GONE_LIMIT);
        drop(host);
        let w = TestWindow::open(TITLE, GREEN, x, y, Kind::Normal);
        let came_back = cx.recording_green(&format!("{name}-reopened"));
        out.push(Outcome { case, noticed, clip_black, came_back });
        drop(w);
        cx.settle();
    }

    assert!(cx.s.is_running(), "recorder error: {:?}", cx.s.error());
    rec.stop();
    println!("\n{:<48} {:>10} {:>11} {:>10}", "case", "noticed", "clip black", "came back");
    for o in &out {
        let noticed = o.noticed.map(|d| format!("{:.1} s", d.as_secs_f64())).unwrap_or_else(|| "NO".into());
        println!("{:<48} {:>10} {:>11} {:>10}   {}", o.case, noticed, o.clip_black, o.came_back, if o.ok() { "ok" } else { "FAIL" });
    }
    println!("clips in {}", cx.clips.display());
    let failed: Vec<&str> = out.iter().filter(|o| !o.ok()).map(|o| o.case).collect();
    assert!(failed.is_empty(), "cases that failed: {failed:?}");
}
