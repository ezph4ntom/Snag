//! Small Win32 helpers shared by several modules.

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;

use windows::Win32::System::SystemInformation::GetLocalTime;

/// `CREATE_NO_WINDOW`: child console programs (ffmpeg/ffprobe) must not flash
/// a console window, because our own exe is a GUI-subsystem program.
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Process priority classes (from WinBase.h).
pub const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
pub const ABOVE_NORMAL_PRIORITY_CLASS: u32 = 0x0000_8000;

/// UTF-16, NUL-terminated copy of a string for passing to `W` Win32 APIs.
pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A `Command` that never shows a console window.
pub fn hidden_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut cmd = Command::new(program);
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// Local time formatted for file names: `2026-11-11_14-03-22`.
pub fn file_timestamp() -> String {
    // SAFETY: GetLocalTime has no preconditions.
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

/// Local time formatted for log lines: `2026-11-11 14:03:22.123`.
pub fn log_timestamp() -> String {
    // SAFETY: GetLocalTime has no preconditions.
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

/// Returns `dir/stem.ext`, or `dir/stem_2.ext`, `dir/stem_3.ext`... if taken.
pub fn unique_path(dir: &Path, stem: &str, ext: &str) -> std::path::PathBuf {
    let first = dir.join(format!("{stem}.{ext}"));
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|n| dir.join(format!("{stem}_{n}.{ext}")))
        .find(|p| !p.exists())
        .expect("infinite iterator")
}

/// Opens a folder in Explorer (or, given a file, opens its folder with the file selected).
pub fn reveal_in_explorer(path: &Path) {
    let result = if path.is_file() {
        Command::new("explorer.exe")
            .raw_arg(format!("/select,\"{}\"", path.display()))
            .spawn()
    } else {
        let _ = std::fs::create_dir_all(path);
        Command::new("explorer.exe").arg(path).spawn()
    };
    if let Err(e) = result {
        log::error!("could not open Explorer for {}: {e}", path.display());
    }
}

/// An anonymous pipe with a large kernel buffer: `(read_end, write_end)`.
///
/// Rust's `Stdio::piped()` uses 4 KB pipe buffers. Pushing 3 MB video frames
/// through that means hundreds of tiny writes per frame; a buffer that holds
/// a whole frame lets each frame go through in one or two system calls.
/// Both ends are non-inheritable; `Command` duplicates the end it hands to
/// the child as inheritable.
pub fn big_pipe(size: u32) -> std::io::Result<(std::fs::File, std::fs::File)> {
    use std::os::windows::io::FromRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Pipes::CreatePipe;
    let mut read = HANDLE::default();
    let mut write = HANDLE::default();
    // SAFETY: out-params are valid; on success we take ownership of both handles.
    unsafe {
        CreatePipe(&mut read, &mut write, None, size).map_err(std::io::Error::other)?;
        Ok((
            std::fs::File::from_raw_handle(read.0 as _),
            std::fs::File::from_raw_handle(write.0 as _),
        ))
    }
}

/// Waits for a child up to `timeout`, killing it if it doesn't exit.
/// Returns the exit status if it exited on its own.
pub fn wait_timeout(
    child: &mut std::process::Child,
    timeout: std::time::Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// True if the Windows taskbar uses the light theme (Settings >
/// Personalization > Colors > "Choose your default Windows mode" = Light).
pub fn taskbar_is_light() -> bool {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    use windows::core::w;
    let mut value = 0u32;
    let mut len = 4u32;
    // SAFETY: reading one DWORD into a 4-byte buffer.
    let ok = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut _),
            Some(&mut len),
        )
    }
    .is_ok();
    ok && value == 1
}

/// Shows a modal error dialog (for fatal startup errors in GUI builds,
/// where there's no console to print to).
pub fn error_box(text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    use windows::core::{PCWSTR, w};
    let text = to_wide(text);
    // SAFETY: valid NUL-terminated strings.
    unsafe {
        MessageBoxW(None, PCWSTR(text.as_ptr()), w!("Snag"), MB_OK | MB_ICONERROR);
    }
}

/// Opts the process into per-monitor DPI awareness so monitor rectangles and
/// capture sizes are reported in physical pixels. Must run before any window
/// is created. Failure is harmless (it just means it was already set).
pub fn set_dpi_awareness() {
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    // SAFETY: plain Win32 call with a constant argument.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}
