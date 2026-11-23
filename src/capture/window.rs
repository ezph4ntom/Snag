//! App windows for "capture only this app": the list the Live tab's picker
//! shows, and finding the chosen app's window again (by exe name) after it
//! restarts.

use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetClassNameW, GetWindow, GetWindowLongW, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsHungAppWindow, IsIconic, IsWindow,
    IsWindowVisible, WS_EX_TOOLWINDOW,
};
use windows::core::BOOL;

/// A top-level window someone could want to record.
#[derive(Clone, Debug, PartialEq)]
pub struct AppWindow {
    /// The HWND as an integer (raw handles aren't `Send`).
    pub hwnd: isize,
    pub title: String,
    /// `game.exe`
    pub exe: String,
    pub pid: u32,
}

/// Visible, titled, unowned app windows in Z order (front first). Skips
/// Snag itself, tool windows, cloaked windows (other virtual desktops,
/// suspended store apps) and the desktop.
pub fn list() -> Vec<AppWindow> {
    let mut handles: Vec<isize> = Vec::new();
    // SAFETY: the callback only pushes into the Vec passed through lparam,
    // which outlives the EnumWindows call.
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut handles as *mut Vec<isize> as isize));
    }
    handles
        .into_iter()
        .filter_map(|h| {
            let hwnd = HWND(h as *mut _);
            if !wanted(hwnd) {
                return None;
            }
            let mut pid = 0u32;
            // SAFETY: plain query on a window handle.
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
            let exe = crate::win::process_exe_name(pid)?;
            if exe.eq_ignore_ascii_case("snag.exe") {
                return None;
            }
            Some(AppWindow { hwnd: h, title: title(hwnd), exe, pid })
        })
        .collect()
}

/// The window to record for the app `exe`: the one titled `title` if it
/// has several, else the frontmost one. One that is "not responding" is
/// picked only if the app has no other: a game busy loading still draws
/// (`WgcSource` notices one that really froze).
pub fn find(exe: &str, title: &str) -> Option<isize> {
    if exe.is_empty() {
        return None;
    }
    let mine: Vec<AppWindow> = list().into_iter().filter(|w| w.exe.eq_ignore_ascii_case(exe)).collect();
    let pick = |c: &[&AppWindow]| c.iter().find(|w| w.title == title).or(c.first()).map(|w| w.hwnd);
    let responding: Vec<&AppWindow> = mine.iter().filter(|w| !is_hung(w.hwnd)).collect();
    pick(&responding).or_else(|| pick(&mine.iter().collect::<Vec<_>>()))
}

pub fn exists(hwnd: isize) -> bool {
    // SAFETY: IsWindow accepts any value.
    unsafe { IsWindow(Some(HWND(hwnd as *mut _))).as_bool() }
}

/// Why a captured window shows nothing new, or None while it's fine
/// (minimized counts as fine: the capture resumes when it's restored).
/// Windows Graphics Capture just stops sending frames for a window that is
/// hidden (a game shutting down slowly, e.g. FiveM after `quit`), on another
/// virtual desktop, or hung, and never says so; recording on would repeat
/// its last frame forever.
pub fn not_showing(hwnd: isize) -> Option<&'static str> {
    let h = HWND(hwnd as *mut _);
    // SAFETY: plain queries; they accept stale handles.
    unsafe {
        if !IsWindow(Some(h)).as_bool() {
            Some("closed")
        } else if !IsWindowVisible(h).as_bool() {
            Some("was hidden")
        } else if is_cloaked(h) {
            Some("isn't on this desktop")
        } else if is_hung(hwnd) {
            Some(NOT_RESPONDING)
        } else {
            None
        }
    }
}

/// What `not_showing` says about a window that is "not responding".
pub const NOT_RESPONDING: &str = "stopped responding";

/// "Not responding": no messages handled for 5 s (Windows' own rule). A
/// game can be that while it keeps drawing (busy loading).
pub fn is_hung(hwnd: isize) -> bool {
    // SAFETY: plain query on a window handle; accepts stale handles.
    unsafe { IsHungAppWindow(HWND(hwnd as *mut _)).as_bool() }
}

/// Hidden by the system although "visible": another virtual desktop, or a
/// suspended store app.
fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked = 0u32;
    // SAFETY: a 4-byte out-param, as DWMWA_CLOAKED requires.
    let got = unsafe { DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut _, 4) };
    got.is_ok() && cloaked != 0
}

pub fn is_minimized(hwnd: isize) -> bool {
    // SAFETY: plain query on a window handle.
    unsafe { IsIconic(HWND(hwnd as *mut _)).as_bool() }
}

/// The window covers its whole monitor (fullscreen or borderless).
pub fn covers_monitor(hwnd: isize) -> bool {
    let Some(m) = super::monitor_info(windows::Win32::Graphics::Gdi::HMONITOR(super::monitor_of_window(HWND(hwnd as *mut _)) as *mut _))
    else {
        return false;
    };
    let r = rect(HWND(hwnd as *mut _));
    r.left <= m.x && r.top <= m.y && r.right >= m.x + m.width as i32 && r.bottom >= m.y + m.height as i32
}

pub fn title(hwnd: HWND) -> String {
    // SAFETY: the buffer is sized from GetWindowTextLengthW (+1 for the NUL).
    unsafe {
        let len = GetWindowTextLengthW(hwnd).max(0) as usize;
        let mut buf = vec![0u16; len + 1];
        let n = GetWindowTextW(hwnd, &mut buf).max(0) as usize;
        String::from_utf16_lossy(&buf[..n])
    }
}

fn rect(hwnd: HWND) -> RECT {
    let mut r = RECT::default();
    // SAFETY: out-param.
    unsafe {
        let _ = GetWindowRect(hwnd, &mut r);
    }
    r
}

fn wanted(hwnd: HWND) -> bool {
    // SAFETY: plain queries on a window handle (it may have closed since
    // EnumWindows; then they just fail).
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || GetWindowTextLengthW(hwnd) == 0 {
            return false;
        }
        if GetWindow(hwnd, GW_OWNER).is_ok_and(|o| !o.0.is_null()) {
            return false; // dialogs and popups of another window
        }
        if GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0 {
            return false;
        }
        if is_cloaked(hwnd) {
            return false;
        }
        let mut class = [0u16; 64];
        let n = GetClassNameW(hwnd, &mut class).max(0) as usize;
        let class = String::from_utf16_lossy(&class[..n]);
        if matches!(class.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd") {
            return false;
        }
    }
    let r = rect(hwnd);
    r.right > r.left && r.bottom > r.top
}

unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: lparam is the &mut Vec passed by `list`.
    let out = unsafe { &mut *(lparam.0 as *mut Vec<isize>) };
    out.push(hwnd.0 as isize);
    BOOL(1)
}
