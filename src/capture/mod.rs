//! Screen capture with the Windows Graphics Capture (WGC) API.
//!
//! * [`d3d`]: creates the Direct3D 11 device everything GPU-side runs on.
//! * [`wgc`]: one WGC session for one monitor; keeps only the newest frame.
//! * [`convert`]: GPU color conversion + scaling (BGRA -> NV12) and readback.
//! * [`screenshot`]: PNG screenshots.
//!
//! Frames stay on the GPU from capture through color conversion and scaling.
//! Only the final, encoder-sized NV12 image (1.5 bytes/pixel instead of 4) is
//! copied to system memory, because the v1 encoder is an ffmpeg sidecar
//! process that reads frames from a pipe. A future in-process encoder
//! (Media Foundation / NVENC SDK) could take the NV12 texture directly; see
//! `encode::VideoEncoder`.

pub mod convert;
pub mod d3d;
pub mod screenshot;
pub mod wgc;

use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW,
    HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MONITORINFOEXW, MonitorFromWindow,
};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;
use windows::core::{BOOL, PCWSTR};

/// A monitor as reported by GDI. `handle` is the `HMONITOR` stored as an
/// integer: raw Win32 handles are pointers and therefore not `Send` in Rust,
/// but monitor handles are plain identifiers that are safe to pass around.
#[derive(Clone, Debug, PartialEq)]
pub struct MonitorInfo {
    pub handle: isize,
    /// GDI device name, e.g. `\\.\DISPLAY1`. Stable across reboots, so this
    /// is what settings store for "fixed monitor" mode.
    pub device_name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
    pub refresh_hz: u32,
}

impl MonitorInfo {
    pub fn hmonitor(&self) -> HMONITOR {
        HMONITOR(self.handle as *mut _)
    }

    /// "DISPLAY2  2560x1440 @ 144 Hz (primary)"
    pub fn label(&self) -> String {
        format!(
            "{}  {}x{} @ {} Hz{}",
            self.device_name.trim_start_matches(r"\\.\"),
            self.width,
            self.height,
            self.refresh_hz,
            if self.primary { " (primary)" } else { "" }
        )
    }
}

/// Lists all active monitors, primary first.
pub fn enumerate_monitors() -> Vec<MonitorInfo> {
    unsafe extern "system" fn callback(hmon: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the `&mut Vec` we passed below, valid for the
        // duration of EnumDisplayMonitors (which calls us synchronously).
        let list = unsafe { &mut *(data.0 as *mut Vec<MonitorInfo>) };
        if let Some(info) = monitor_info(hmon) {
            list.push(info);
        }
        BOOL(1) // continue enumeration
    }

    let mut list: Vec<MonitorInfo> = Vec::new();
    // SAFETY: callback only touches `list` through the pointer we pass.
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(callback), LPARAM(&mut list as *mut _ as isize));
    }
    list.sort_by_key(|m| (!m.primary, m.x, m.y));
    list
}

/// Queries one monitor's geometry, device name and refresh rate.
pub fn monitor_info(hmon: HMONITOR) -> Option<MonitorInfo> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: MONITORINFOEXW starts with MONITORINFO and cbSize tells the API
    // which variant it got.
    let ok = unsafe { GetMonitorInfoW(hmon, &mut info as *mut _ as *mut MONITORINFO) };
    if !ok.as_bool() {
        return None;
    }
    let dev = &info.szDevice;
    let len = dev.iter().position(|&c| c == 0).unwrap_or(dev.len());
    let device_name = String::from_utf16_lossy(&dev[..len]);

    let mut mode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: szDevice is NUL-terminated; mode.dmSize is set.
    let refresh_hz = unsafe {
        if EnumDisplaySettingsW(PCWSTR(dev.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode).as_bool() {
            mode.dmDisplayFrequency.max(1)
        } else {
            60
        }
    };
    let r = info.monitorInfo.rcMonitor;
    Some(MonitorInfo {
        handle: hmon.0 as isize,
        device_name,
        x: r.left,
        y: r.top,
        width: (r.right - r.left).max(0) as u32,
        height: (r.bottom - r.top).max(0) as u32,
        primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        refresh_hz,
    })
}

/// The monitor that contains most of `hwnd`.
pub fn monitor_of_window(hwnd: HWND) -> isize {
    // SAFETY: MonitorFromWindow accepts any HWND, valid or not.
    unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST).0 as isize }
}

pub fn primary_monitor() -> Option<MonitorInfo> {
    enumerate_monitors().into_iter().find(|m| m.primary)
}

/// Finds a monitor by GDI device name; falls back to the primary monitor.
pub fn monitor_by_name(name: &str) -> Option<MonitorInfo> {
    let all = enumerate_monitors();
    all.iter()
        .find(|m| !name.is_empty() && m.device_name.eq_ignore_ascii_case(name))
        .or_else(|| all.iter().find(|m| m.primary))
        .or_else(|| all.first())
        .cloned()
}

/// Encoder output size for a given height setting, using the primary
/// monitor's aspect ratio. `0` means native. Both dimensions are even, which
/// NV12 (4:2:0 chroma) requires.
pub fn output_size(output_height: u32) -> (u32, u32) {
    let (mw, mh) = primary_monitor()
        .map(|m| (m.width.max(2), m.height.max(2)))
        .unwrap_or((1920, 1080));
    if output_height == 0 || output_height >= mh {
        return (mw & !1, mh & !1);
    }
    let w = ((mw as u64 * output_height as u64) / mh as u64) as u32;
    (w.max(2) & !1, output_height & !1)
}
