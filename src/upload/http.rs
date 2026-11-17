//! The small HTTP layer the uploaders share: an agent using Windows' own
//! TLS (schannel) and certificate store, multipart bodies streamed straight
//! from the file with progress, and cancellation.

use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
use ureq::{Agent, SendBody};

/// What a browser sends; Streamable's web endpoints expect one.
pub const BROWSER_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// A fresh agent with its own cookie jar.
pub fn agent(user_agent: &str) -> Agent {
    Agent::config_builder()
        .tls_config(TlsConfig::builder().provider(TlsProvider::NativeTls).root_certs(RootCerts::PlatformVerifier).build())
        .user_agent(user_agent)
        // Error statuses come back as responses, so their message can be shown.
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_response(Some(Duration::from_secs(180)))
        .timeout_recv_body(Some(Duration::from_secs(60)))
        .build()
        .into()
}

/// Progress and cancellation shared between an upload and the window.
#[derive(Default)]
pub struct Progress {
    pub sent: AtomicU64,
    pub total: AtomicU64,
    pub cancel: AtomicBool,
}

impl Progress {
    pub fn fraction(&self) -> f32 {
        let total = self.total.load(Ordering::Relaxed);
        if total == 0 { 0.0 } else { (self.sent.load(Ordering::Relaxed) as f64 / total as f64).min(1.0) as f32 }
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// Counts bytes as they are read (i.e. sent) and stops on cancel.
struct Counting<R> {
    inner: R,
    progress: Arc<Progress>,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.progress.cancelled() {
            return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "upload cancelled"));
        }
        let n = self.inner.read(buf)?;
        self.progress.sent.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

/// A multipart/form-data body: text fields, then one file, streamed.
pub struct Multipart {
    boundary: String,
    head: Vec<u8>,
}

impl Default for Multipart {
    fn default() -> Self {
        Self::new()
    }
}

impl Multipart {
    pub fn new() -> Multipart {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        Multipart { boundary: format!("----SnagFormBoundary{nanos:x}"), head: Vec::new() }
    }

    pub fn text(mut self, name: &str, value: &str) -> Multipart {
        self.head.extend_from_slice(
            format!("--{}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n", self.boundary).as_bytes(),
        );
        self
    }

    /// POSTs the fields plus `file` (as form field `field`) to `url`.
    /// Returns (status, response text).
    pub fn post_file(
        mut self,
        agent: &Agent,
        url: &str,
        field: &str,
        file: &Path,
        mime: &str,
        progress: &Arc<Progress>,
    ) -> Result<(u16, String)> {
        let name = file.file_name().unwrap_or_default().to_string_lossy().replace('"', "");
        self.head.extend_from_slice(
            format!(
                "--{}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{name}\"\r\nContent-Type: {mime}\r\n\r\n",
                self.boundary
            )
            .as_bytes(),
        );
        let tail = format!("\r\n--{}--\r\n", self.boundary).into_bytes();
        let f = std::fs::File::open(file).with_context(|| format!("opening {}", file.display()))?;
        let file_len = f.metadata()?.len();
        let total = self.head.len() as u64 + file_len + tail.len() as u64;
        progress.total.store(total, Ordering::Relaxed);
        progress.sent.store(0, Ordering::Relaxed);
        let reader = Cursor::new(std::mem::take(&mut self.head))
            .chain(std::io::BufReader::with_capacity(256 * 1024, f))
            .chain(Cursor::new(tail));
        let mut body = Counting { inner: reader, progress: progress.clone() };
        let req = agent
            .post(url)
            .header("Content-Type", format!("multipart/form-data; boundary={}", self.boundary))
            // A known length (not chunked): S3 refuses chunked form uploads.
            .header("Content-Length", total.to_string());
        let result = req.send(SendBody::from_reader(&mut body));
        if progress.cancelled() {
            bail!("cancelled");
        }
        let mut resp = result.with_context(|| format!("uploading to {}", host_of(url)))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        Ok((status, text))
    }
}

/// `https://a.b.com/x` -> `a.b.com` (for messages).
pub fn host_of(url: &str) -> &str {
    url.split("://").nth(1).unwrap_or(url).split('/').next().unwrap_or(url)
}

/// The first 300 characters of a response, on one line (for errors).
pub fn snippet(text: &str) -> String {
    let one: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    one.chars().take(300).collect()
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    use super::*;

    /// The form is sent with a Content-Length (S3 refuses chunked uploads),
    /// is well-formed, carries the whole file, and progress reaches 100 %.
    #[test]
    fn multipart_upload_is_sized_and_complete() {
        let dir = std::env::temp_dir().join(format!("snag-mp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("clip.mp4");
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&file, &data).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut r = BufReader::new(stream.try_clone().unwrap());
            let mut headers = Vec::new();
            loop {
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                headers.push(line.trim().to_lowercase());
            }
            let len: usize = headers.iter().find_map(|h| h.strip_prefix("content-length: ")).unwrap().parse().unwrap();
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            let mut w = stream;
            w.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").unwrap();
            (headers, body)
        });

        let progress = Arc::new(Progress::default());
        let (status, text) = Multipart::new()
            .text("key", "uploads/abc")
            .post_file(&agent("test"), &format!("http://127.0.0.1:{port}/up"), "file", &file, "video/mp4", &progress)
            .unwrap();
        assert_eq!((status, text.as_str()), (200, "ok"));
        let (headers, body) = server.join().unwrap();
        assert!(!headers.iter().any(|h| h.starts_with("transfer-encoding")), "{headers:?}");
        let ct = headers.iter().find_map(|h| h.strip_prefix("content-type: multipart/form-data; boundary=")).unwrap().to_string();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("name=\"key\"\r\n\r\nuploads/abc\r\n"));
        assert!(text.contains("name=\"file\"; filename=\"clip.mp4\"\r\nContent-Type: video/mp4\r\n\r\n"));
        assert!(text.to_lowercase().ends_with(&format!("\r\n--{ct}--\r\n")), "boundary at the end");
        let start = body.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let file_start = body[start + 4..].windows(4).position(|w| w == b"\r\n\r\n").unwrap() + start + 8;
        assert_eq!(&body[file_start..file_start + data.len()], &data[..]);
        assert!((progress.fraction() - 1.0).abs() < 1e-6);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// TLS through Windows (schannel) and its certificate store works, with
    /// read-only requests (nothing is uploaded).
    #[test]
    #[ignore = "needs the internet"]
    fn reaches_the_hosts() {
        let a = agent(BROWSER_UA);
        let r = a.get("https://litterbox.catbox.moe/").call().unwrap();
        assert!(r.status().is_success(), "litterbox: {}", r.status());
        let r = a.get("https://api.streamable.com/videos/snag-does-not-exist").call().unwrap();
        assert!(r.status().as_u16() == 404 || r.status().is_success(), "streamable: {}", r.status());
    }
}
