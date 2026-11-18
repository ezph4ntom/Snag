//! A tiny file logger behind the standard `log` facade.
//!
//! Why not `env_logger`/`tracing`? We need a file (release builds have no
//! console) and almost nothing else, so ~50 lines beat a dependency tree.
//! Each process writes its own file (`core.log`, `ui.log`) in
//! `%LOCALAPPDATA%\Snag\logs`; files over 5 MB are rotated to `.old.log`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::Mutex;

use log::{Level, LevelFilter, Log, Metadata, Record};

struct FileLogger {
    file: Mutex<Option<File>>,
    /// The user's profile folder, written as %USERPROFILE% in log lines:
    /// logs get attached to public bug reports, and the folder name is
    /// usually the person's name.
    home: Option<String>,
    also_stderr: bool,
    level: LevelFilter,
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        // Keep noisy dependencies (winit, glow, ...) at warn and above.
        let ours = metadata.target().starts_with("snag");
        metadata.level() <= self.level && (ours || metadata.level() <= Level::Warn)
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let target = record.target().trim_start_matches("snag::");
        let message = record.args().to_string();
        let message = match &self.home {
            Some(home) => mask_home(&message, home),
            None => message,
        };
        let line = format!("{} {:<5} [{}] {}\n", crate::win::log_timestamp(), record.level(), target, message);
        if self.also_stderr {
            eprint!("{line}");
        }
        if let Ok(mut guard) = self.file.lock() {
            if let Some(f) = guard.as_mut() {
                let _ = f.write_all(line.as_bytes());
            }
        }
    }

    fn flush(&self) {
        if let Ok(mut guard) = self.file.lock() {
            if let Some(f) = guard.as_mut() {
                let _ = f.flush();
            }
        }
    }
}

/// `text` with every `home` (any letter case, either slash) replaced by
/// `%USERPROFILE%`.
fn mask_home(text: &str, home: &str) -> String {
    let home = home.trim_end_matches(['\\', '/']);
    if home.len() < 4 {
        return text.to_string();
    }
    // Compare with one kind of slash; ASCII lowercasing keeps byte offsets.
    let norm = |s: &str| s.replace('/', "\\").to_ascii_lowercase();
    let (hay, needle) = (norm(text), norm(home));
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (i, _) in hay.match_indices(&needle) {
        out.push_str(&text[last..i]);
        out.push_str("%USERPROFILE%");
        last = i + needle.len();
    }
    out.push_str(&text[last..]);
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn hides_the_user_folder() {
        let home = r"C:\Users\Jane Doe";
        let line = r"saved C:\Users\Jane Doe\Videos\Snag\Clip.mp4 and c:/users/jane doe/AppData/x";
        assert_eq!(super::mask_home(line, home), r"saved %USERPROFILE%\Videos\Snag\Clip.mp4 and %USERPROFILE%/AppData/x");
        assert_eq!(super::mask_home("nothing here", home), "nothing here");
        assert_eq!(super::mask_home(r"Ärger in C:\Users\Jane Doe\ü", home), r"Ärger in %USERPROFILE%\ü");
    }
}

/// Installs the logger. `name` becomes the file name (`core` -> `core.log`).
/// Set `SNAG_LOG=debug` for verbose logs.
pub fn init(name: &str) {
    let dir = crate::paths::log_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{name}.log"));
    if std::fs::metadata(&path).map(|m| m.len() > 5 * 1024 * 1024).unwrap_or(false) {
        let _ = std::fs::rename(&path, dir.join(format!("{name}.old.log")));
    }
    let file = OpenOptions::new().create(true).append(true).open(&path).ok();
    let level = match std::env::var("SNAG_LOG").as_deref() {
        Ok("trace") => LevelFilter::Trace,
        Ok("debug") => LevelFilter::Debug,
        Ok("warn") => LevelFilter::Warn,
        _ => LevelFilter::Info,
    };
    let logger = FileLogger {
        file: Mutex::new(file),
        home: std::env::var("USERPROFILE").ok(),
        also_stderr: cfg!(debug_assertions) || std::env::var_os("SNAG_LOG_STDERR").is_some(),
        level,
    };
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(level);
    }
    // Log panics from any thread before the default hook prints them.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("PANIC: {info}");
        log::logger().flush();
        default_hook(info);
    }));
}
