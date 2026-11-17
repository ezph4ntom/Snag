//! Litterbox (litterbox.catbox.moe): anonymous temporary file hosting, no
//! account. Files up to 1 GB, kept for 1, 12, 24 or 72 hours.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};

use super::http::{self, Multipart, Progress};

const API: &str = "https://litterbox.catbox.moe/resources/internals/api.php";
pub const MAX_BYTES: u64 = 1000 * 1000 * 1000;
pub const HOURS: [u32; 4] = [1, 12, 24, 72];

/// Uploads `file`, kept for `hours`; returns its link.
pub fn upload(file: &Path, hours: u32, progress: &Arc<Progress>) -> Result<String> {
    let hours = if HOURS.contains(&hours) { hours } else { 72 };
    let agent = http::agent(concat!("Snag/", env!("CARGO_PKG_VERSION")));
    let (status, text) = Multipart::new()
        .text("reqtype", "fileupload")
        .text("time", &format!("{hours}h"))
        .post_file(&agent, API, "fileToUpload", file, "video/mp4", progress)?;
    let text = text.trim();
    if !(200..300).contains(&status) || !text.starts_with("https://") {
        bail!("Litterbox refused the upload (HTTP {status}): {}", http::snippet(text));
    }
    Ok(text.to_string())
}
