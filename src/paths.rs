//! Where everything lives on disk.
//!
//! * Settings:        `%APPDATA%\Snag\settings.json` (roams with the user profile)
//! * Buffer + logs:   `%LOCALAPPDATA%\Snag\` (machine-local, never roams)
//! * Clips:           `%USERPROFILE%\Videos\Snag` by default
//! * Screenshots:     `%USERPROFILE%\Pictures\Snag` by default
//! * Resources:       `themes\`, `assets\`, `bin\ffmpeg.exe` next to the exe
//!   (or in the project root when running via `cargo run`)

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    FOLDERID_LocalAppData, FOLDERID_Pictures, FOLDERID_RoamingAppData, FOLDERID_Videos,
    KF_FLAG_DEFAULT, SHGetKnownFolderPath,
};
use windows::core::GUID;

pub const APP_NAME: &str = "Snag";

fn known_folder(id: &GUID) -> Option<PathBuf> {
    // SAFETY: SHGetKnownFolderPath allocates a string we must free with
    // CoTaskMemFree; we copy it into a Rust String before freeing.
    unsafe {
        let pwstr = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let s = pwstr.to_string().ok();
        CoTaskMemFree(Some(pwstr.0 as *const _));
        s.map(PathBuf::from)
    }
}

fn env_dir(var: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// `%APPDATA%\Snag` (settings).
pub fn config_dir() -> PathBuf {
    known_folder(&FOLDERID_RoamingAppData)
        .unwrap_or_else(|| env_dir("APPDATA"))
        .join(APP_NAME)
}

/// Snag was called "ClipApp" before v0.3. If this PC has ClipApp settings
/// but no Snag settings yet, copy them over (the old file stays as it was).
/// Folder paths stored in them (e.g. `Videos\ClipApp`) are kept, so existing
/// clips stay where they are.
pub fn migrate_from_clipapp() {
    let new = settings_file();
    if new.exists() {
        return;
    }
    let old = known_folder(&FOLDERID_RoamingAppData)
        .unwrap_or_else(|| env_dir("APPDATA"))
        .join("ClipApp")
        .join("settings.json");
    if old.is_file() && std::fs::create_dir_all(config_dir()).is_ok() && std::fs::copy(&old, &new).is_ok() {
        log::info!("imported settings from {}", old.display());
    }
}

/// `%LOCALAPPDATA%\Snag` (buffer, logs).
pub fn local_dir() -> PathBuf {
    known_folder(&FOLDERID_LocalAppData)
        .unwrap_or_else(|| env_dir("LOCALAPPDATA"))
        .join(APP_NAME)
}

pub fn settings_file() -> PathBuf {
    config_dir().join("settings.json")
}

pub fn buffer_root() -> PathBuf {
    local_dir().join("buffer")
}

/// Recordings in progress (moved to the clips folder when saved).
pub fn recordings_dir() -> PathBuf {
    local_dir().join("recordings")
}

/// Cached clip thumbnails for the Live tab.
pub fn thumbs_dir() -> PathBuf {
    local_dir().join("thumbs")
}

pub fn log_dir() -> PathBuf {
    local_dir().join("logs")
}

pub fn default_clips_dir() -> PathBuf {
    known_folder(&FOLDERID_Videos)
        .unwrap_or_else(|| env_dir("USERPROFILE").join("Videos"))
        .join(APP_NAME)
}

pub fn default_screenshots_dir() -> PathBuf {
    known_folder(&FOLDERID_Pictures)
        .unwrap_or_else(|| env_dir("USERPROFILE").join("Pictures"))
        .join(APP_NAME)
}

pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Folder that contains `themes\` and `assets\`.
///
/// A release install keeps them next to the exe. During development the exe
/// lives in `target\debug\`, so we also walk up a few parent folders and
/// finally try the current directory.
pub fn resource_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        if let Some(dir) = std::env::var_os("SNAG_RESOURCES") {
            return PathBuf::from(dir);
        }
        let exe = exe_dir();
        let mut candidates: Vec<PathBuf> = exe.ancestors().take(4).map(Path::to_path_buf).collect();
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd);
        }
        candidates
            .into_iter()
            .find(|d| d.join("themes").is_dir() || d.join("assets").is_dir())
            .unwrap_or(exe)
    })
}

pub fn themes_dir() -> PathBuf {
    resource_dir().join("themes")
}

pub fn clip_sound_file() -> PathBuf {
    resource_dir().join("assets").join("sounds").join("clip.wav")
}

/// Finds a sidecar tool (`ffmpeg` or `ffprobe`).
///
/// Search order: `SNAG_FFMPEG_DIR` env var, `bin\` next to the resources,
/// next to the exe, every folder on `PATH`, then the places a tool installed
/// after this process started shows up: winget's links folder and the PATH
/// saved in the registry (our inherited PATH is a copy from when we started).
fn find_tool(name: &str) -> Option<PathBuf> {
    let file = format!("{name}.exe");
    find_in(&tool_dirs(), &file).or_else(|| find_in(&late_tool_dirs(), &file))
}

/// The folders searched first (see `find_tool`).
fn tool_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = std::env::var_os("SNAG_FFMPEG_DIR") {
        dirs.push(PathBuf::from(d));
    }
    dirs.push(resource_dir().join("bin"));
    dirs.push(exe_dir().join("bin"));
    dirs.push(exe_dir());
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    dirs
}

/// Where a tool installed after this process started shows up.
fn late_tool_dirs() -> Vec<PathBuf> {
    let winget = std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join(r"Microsoft\WinGet\Links"));
    winget.into_iter().chain(registry_path_dirs()).collect()
}

/// The first `dir\file` that is a file. A symbolic link counts only if what
/// it points to exists (winget installs ffmpeg as links).
fn find_in(dirs: &[PathBuf], file: &str) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join(file)).find(|p| p.is_file())
}

/// `dir\file` entries that are links to something that no longer exists,
/// with where they point: a half-finished winget upgrade leaves these.
/// Judged by the target itself, not by following the link: a process that
/// inherited an installer's RedirectionGuard can't follow winget's links
/// (error 448) although they're fine.
fn broken_links_in(dirs: &[PathBuf], file: &str) -> Vec<(PathBuf, PathBuf)> {
    let mut out: Vec<(PathBuf, PathBuf)> = Vec::new();
    for link in dirs.iter().map(|d| d.join(file)) {
        let is_link = std::fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink());
        let Some(target) = is_link.then(|| std::fs::read_link(&link).ok()).flatten() else { continue };
        // A relative target is relative to the link's folder.
        let full = link.parent().map_or(target.clone(), |d| d.join(&target));
        if std::fs::symlink_metadata(&full).is_err() && !out.iter().any(|(l, _)| *l == link) {
            out.push((link, target));
        }
    }
    out
}

/// Broken links named like the tool where Snag looks for it, with their
/// targets (for telling the user why ffmpeg isn't found).
pub fn broken_tool_links(name: &str) -> Vec<(PathBuf, PathBuf)> {
    let dirs: Vec<PathBuf> = tool_dirs().into_iter().chain(late_tool_dirs()).collect();
    broken_links_in(&dirs, &format!("{name}.exe"))
}

/// A found tool, remembered; while it isn't found, the bare name (so the
/// error from `Command::spawn` still says what is missing) and every call
/// looks again: ffmpeg installed while Snag runs is picked up without a
/// restart.
fn tool(slot: &Mutex<Option<&'static Path>>, name: &str, bare: &'static str) -> &'static Path {
    let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = *slot {
        return p;
    }
    match find_tool(name) {
        // Leaked once per find (normally once per run) to stay &'static.
        Some(p) => {
            let p: &'static Path = Box::leak(p.into_boxed_path());
            *slot = Some(p);
            p
        }
        None => Path::new(bare),
    }
}

static FFMPEG: Mutex<Option<&'static Path>> = Mutex::new(None);
static FFPROBE: Mutex<Option<&'static Path>> = Mutex::new(None);

pub fn ffmpeg_exe() -> &'static Path {
    tool(&FFMPEG, "ffmpeg", "ffmpeg.exe")
}

pub fn ffprobe_exe() -> &'static Path {
    tool(&FFPROBE, "ffprobe", "ffprobe.exe")
}

/// Forget where ffmpeg/ffprobe were found (they failed to run: maybe moved
/// or reinstalled elsewhere), so the next call looks again.
pub fn forget_tools() {
    for slot in [&FFMPEG, &FFPROBE] {
        *slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// The user + machine `Path` values as currently stored in the registry.
fn registry_path_dirs() -> Vec<PathBuf> {
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
        RegGetValueW,
    };
    use windows::core::PCWSTR;

    fn read(root: HKEY, subkey: &str) -> Option<String> {
        let subkey = crate::win::to_wide(subkey);
        let value = crate::win::to_wide("Path");
        let mut buf = vec![0u16; 32 * 1024];
        let mut len_bytes = (buf.len() * 2) as u32;
        // SAFETY: buffers are valid for the sizes we pass; RegGetValueW
        // expands %VARS% in REG_EXPAND_SZ values for us.
        let status = unsafe {
            RegGetValueW(
                root,
                PCWSTR(subkey.as_ptr()),
                PCWSTR(value.as_ptr()),
                RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut len_bytes),
            )
        };
        if status.is_err() {
            return None;
        }
        let chars = (len_bytes as usize / 2).saturating_sub(1);
        Some(String::from_utf16_lossy(&buf[..chars]))
    }

    let mut out = Vec::new();
    for (root, key) in [
        (HKEY_CURRENT_USER, "Environment"),
        (
            HKEY_LOCAL_MACHINE,
            r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment",
        ),
    ] {
        if let Some(v) = read(root, key) {
            out.extend(v.split(';').filter(|p| !p.is_empty()).map(PathBuf::from));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_the_path_saved_in_the_registry() {
        // The machine PATH always exists (and holds System32): this is how
        // Snag finds an ffmpeg installed after it started.
        let dirs = super::registry_path_dirs();
        assert!(dirs.iter().any(|d| d.join("cmd.exe").is_file()), "registry PATH: {dirs:?}");
        assert!(dirs.iter().all(|d| !d.to_string_lossy().contains('%')), "not expanded: {dirs:?}");
    }

    #[test]
    fn a_broken_link_is_not_a_tool_but_is_reported() {
        // What a half-finished winget upgrade leaves: WinGet\Links\ffmpeg.exe
        // pointing into a version folder that was deleted.
        let dir = std::env::temp_dir().join(format!("snag-broken-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (links, good) = (dir.join("links"), dir.join("good"));
        std::fs::create_dir_all(&links).unwrap();
        std::fs::create_dir_all(&good).unwrap();
        let target = dir.join("ffmpeg-9.0.2").join("ffmpeg.exe");
        if let Err(e) = std::os::windows::fs::symlink_file(&target, links.join("tool.exe")) {
            // Needs Developer Mode (or admin), like winget's own links.
            eprintln!("skipped: can't create symbolic links here ({e})");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let dirs = [links.clone(), good.clone()];
        assert_eq!(super::find_in(&dirs, "tool.exe"), None);
        assert_eq!(super::broken_links_in(&dirs, "tool.exe"), [(links.join("tool.exe"), target.clone())]);
        // A real file further down the list is still found.
        std::fs::write(good.join("tool.exe"), b"").unwrap();
        assert_eq!(super::find_in(&dirs, "tool.exe"), Some(good.join("tool.exe")));
        // Once the link's target is back, the link is the tool again.
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"").unwrap();
        assert_eq!(super::find_in(&dirs, "tool.exe"), Some(links.join("tool.exe")));
        assert!(super::broken_links_in(&dirs, "tool.exe").is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_tool_installed_later_is_found_without_a_restart() {
        use std::path::Path;
        use std::sync::Mutex;
        // A made-up tool next to the test exe (one of the folders searched).
        let name = format!("snag-test-tool-{}", std::process::id());
        let file = super::exe_dir().join(format!("{name}.exe"));
        let slot: Mutex<Option<&'static Path>> = Mutex::new(None);
        let _ = std::fs::remove_file(&file);
        assert_eq!(super::tool(&slot, &name, "missing.exe"), Path::new("missing.exe"));
        assert!(slot.lock().unwrap().is_none(), "a missing tool isn't remembered");
        std::fs::write(&file, b"").unwrap();
        assert_eq!(super::tool(&slot, &name, "missing.exe"), file);
        std::fs::remove_file(&file).unwrap();
        // Remembered once found, until forgotten.
        assert_eq!(super::tool(&slot, &name, "missing.exe"), file);
        *slot.lock().unwrap() = None;
        assert_eq!(super::tool(&slot, &name, "missing.exe"), Path::new("missing.exe"));
    }
}
