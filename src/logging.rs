//! A tiny file logger behind the standard `log` facade.
//!
//! Why not `env_logger`/`tracing`? We need a file (release builds have no
//! console) and almost nothing else, so ~50 lines beat a dependency tree.
//! Each process writes its own file (`core.log`, `ui.log`) in
//! `%LOCALAPPDATA%\ClipApp\logs`; files over 5 MB are rotated to `.old.log`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::Mutex;

use log::{Level, LevelFilter, Log, Metadata, Record};

struct FileLogger {
    file: Mutex<Option<File>>,
    also_stderr: bool,
    level: LevelFilter,
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        // Keep noisy dependencies (winit, glow, ...) at warn and above.
        let ours = metadata.target().starts_with("clipapp");
        metadata.level() <= self.level && (ours || metadata.level() <= Level::Warn)
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let target = record.target().trim_start_matches("clipapp::");
        let line = format!(
            "{} {:<5} [{}] {}\n",
            crate::win::log_timestamp(),
            record.level(),
            target,
            record.args()
        );
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

/// Installs the logger. `name` becomes the file name (`core` -> `core.log`).
/// Set `CLIPAPP_LOG=debug` for verbose logs.
pub fn init(name: &str) {
    let dir = crate::paths::log_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{name}.log"));
    if std::fs::metadata(&path).map(|m| m.len() > 5 * 1024 * 1024).unwrap_or(false) {
        let _ = std::fs::rename(&path, dir.join(format!("{name}.old.log")));
    }
    let file = OpenOptions::new().create(true).append(true).open(&path).ok();
    let level = match std::env::var("CLIPAPP_LOG").as_deref() {
        Ok("trace") => LevelFilter::Trace,
        Ok("debug") => LevelFilter::Debug,
        Ok("warn") => LevelFilter::Warn,
        _ => LevelFilter::Info,
    };
    let logger = FileLogger {
        file: Mutex::new(file),
        also_stderr: cfg!(debug_assertions) || std::env::var_os("CLIPAPP_LOG_STDERR").is_some(),
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
