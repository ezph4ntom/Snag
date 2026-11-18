//! Updates from GitHub releases.
//!
//! The window asks GitHub for the latest release (at most every few hours;
//! the answer is cached in `%LOCALAPPDATA%\Snag\update.json`). When it is
//! newer than this build, the title bar shows "Update to X". Clicking it
//! downloads the release's installer, checks its SHA-256 against the one
//! GitHub lists, and runs it silently: the installer closes Snag, updates
//! it in place and starts it again. Portable copies (no installer) get the
//! release page opened instead.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::upload::http::Progress;

const REPO: &str = "ezph4ntom/Snag";

/// The project's GitHub page.
pub fn source_url() -> String {
    format!("https://github.com/{REPO}")
}

/// This version's release notes on GitHub.
pub fn release_notes_url() -> String {
    format!("https://github.com/{REPO}/releases/tag/v{}", current())
}
/// How often GitHub is asked while the window is open (it allows 60
/// requests an hour per address).
const CHECK_EVERY: Duration = Duration::from_secs(3600);
/// A check younger than this is reused when the window opens; anything
/// older is redone right away, so a new release shows up promptly.
const REUSE_FOR: Duration = Duration::from_secs(30 * 60);
/// A failed check (offline, GitHub down) is tried again after this.
const RETRY_AFTER: Duration = Duration::from_secs(10 * 60);

/// How long after the window opens to ask GitHub, given the age of the
/// last check saved in update.json.
fn first_check_in(age: Duration) -> Duration {
    REUSE_FOR.saturating_sub(age)
}

/// How long after a check finished to ask again.
fn next_check_in(ok: bool) -> Duration {
    if ok { CHECK_EVERY } else { RETRY_AFTER }
}
/// The installer is a few MB; anything far bigger is not ours.
const MAX_INSTALLER_BYTES: u64 = 200 << 20;

/// This build's version.
pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The newest published release.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Release {
    /// "0.6.1" (tag without the "v").
    pub version: String,
    /// Its page on GitHub.
    pub page: String,
    pub installer: Option<Asset>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
    /// Lowercase hex, when GitHub lists one.
    pub sha256: Option<String>,
}

/// `"v1.2.3"` / `"1.2"` -> (1, 2, 3). Anything after a `-` is ignored.
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let s = s.split(['-', '+']).next()?;
    let mut parts = s.split('.').map(|p| p.parse::<u64>().ok());
    let major = parts.next()??;
    let minor = parts.next().unwrap_or(Some(0))?;
    let patch = parts.next().unwrap_or(Some(0))?;
    Some((major, minor, patch))
}

pub fn is_newer(latest: &str, than: &str) -> bool {
    matches!((parse_version(latest), parse_version(than)), (Some(a), Some(b)) if a > b)
}

/// Snag was installed with the installer (its uninstaller sits next to the
/// exe), so the installer can update it in place.
pub fn installed() -> bool {
    crate::paths::exe_dir().join("unins000.exe").exists()
}

fn agent() -> ureq::Agent {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    ureq::Agent::config_builder()
        .tls_config(TlsConfig::builder().provider(TlsProvider::NativeTls).root_certs(RootCerts::PlatformVerifier).build())
        .user_agent(concat!("Snag/", env!("CARGO_PKG_VERSION"), " (update check)"))
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .timeout_recv_body(Some(Duration::from_secs(15 * 60)))
        .build()
        .into()
}

/// Asks GitHub for the latest release.
pub fn fetch_latest() -> Result<Release> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let mut resp = agent()
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .context("could not reach GitHub")?;
    let status = resp.status().as_u16();
    if status == 404 {
        bail!("no releases yet");
    }
    if status != 200 {
        bail!("GitHub answered HTTP {status}");
    }
    let text = resp.body_mut().with_config().limit(2 << 20).read_to_string().context("reading GitHub's answer")?;
    let json: serde_json::Value = serde_json::from_str(&text).context("reading GitHub's answer")?;
    parse_release(&json)
}

fn parse_release(json: &serde_json::Value) -> Result<Release> {
    let tag = json["tag_name"].as_str().context("no tag_name")?;
    let version = tag.trim_start_matches(['v', 'V']).to_string();
    if parse_version(&version).is_none() {
        bail!("unexpected version tag {tag:?}");
    }
    let page = json["html_url"].as_str().map(str::to_string).unwrap_or_else(|| format!("https://github.com/{REPO}/releases/latest"));
    // Only files of this repository's releases are ever downloaded.
    let prefix = format!("https://github.com/{REPO}/releases/download/");
    let installer = json["assets"].as_array().into_iter().flatten().find_map(|a| {
        let name = a["name"].as_str()?;
        let url = a["browser_download_url"].as_str()?;
        let lower = name.to_ascii_lowercase();
        if !(lower.starts_with("snag-setup") && lower.ends_with(".exe") && url.starts_with(&prefix)) {
            return None;
        }
        let sha256 = a["digest"].as_str().and_then(|d| d.strip_prefix("sha256:")).map(|h| h.to_ascii_lowercase());
        Some(Asset { name: name.to_string(), url: url.to_string(), size: a["size"].as_u64().unwrap_or(0), sha256 })
    });
    Ok(Release { version, page, installer })
}

/// Downloads the installer into `%TEMP%\Snag-update` and checks it.
pub fn download(asset: &Asset, progress: &Progress) -> Result<PathBuf> {
    if asset.size > MAX_INSTALLER_BYTES {
        bail!("the installer is unexpectedly large");
    }
    let dir = std::env::temp_dir().join("Snag-update");
    std::fs::create_dir_all(&dir)?;
    // Only a plain file name, never a path.
    let name: String = asset.name.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')).collect();
    let path = dir.join(if name.is_empty() { "Snag-Setup.exe".to_string() } else { name });

    let resp = agent().get(&asset.url).call().context("could not reach GitHub")?;
    let status = resp.status().as_u16();
    if status != 200 {
        bail!("download failed (HTTP {status})");
    }
    progress.total.store(asset.size, Ordering::Relaxed);
    progress.sent.store(0, Ordering::Relaxed);
    let mut reader = resp.into_body().into_with_config().limit(MAX_INSTALLER_BYTES).reader();
    let mut data = Vec::with_capacity(asset.size as usize);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        if progress.cancelled() {
            bail!("cancelled");
        }
        let n = reader.read(&mut buf).context("download interrupted")?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
        progress.sent.fetch_add(n as u64, Ordering::Relaxed);
    }
    if asset.size != 0 && data.len() as u64 != asset.size {
        bail!("the download is incomplete ({} of {} bytes)", data.len(), asset.size);
    }
    if let Some(want) = &asset.sha256 {
        let got = sha256_hex(&data)?;
        if &got != want {
            bail!("the download doesn't match the release (checksum differs)");
        }
    }
    let mut f = std::fs::File::create(&path).with_context(|| format!("writing {}", path.display()))?;
    f.write_all(&data)?;
    f.sync_all()?;
    Ok(path)
}

fn sha256_hex(data: &[u8]) -> Result<String> {
    use windows::Win32::Security::Cryptography::{BCRYPT_SHA256_ALG_HANDLE, BCryptHash};
    let mut out = [0u8; 32];
    // SAFETY: valid slices; the pseudo-handle needs no opening.
    let status = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, data, &mut out) };
    if status.is_err() {
        bail!("could not compute the checksum ({:#x})", status.0);
    }
    Ok(out.iter().map(|b| format!("{b:02x}")).collect())
}

/// Runs the installer silently. It closes Snag, updates the files where
/// Snag is installed and starts it again (`/RELAUNCH=1`).
pub fn run_installer(path: &Path) -> Result<()> {
    use std::process::Stdio;
    std::process::Command::new(path)
        .args(["/SILENT", "/SP-", "/SUPPRESSMSGBOXES", "/NORESTART", "/NOCANCEL", "/RELAUNCH=1"])
        // Not our pipes to the tray app: they must close when this window does.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("starting {}", path.display()))?;
    Ok(())
}

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    checked_at: u64,
    release: Option<Release>,
}

fn cache_file() -> PathBuf {
    crate::paths::local_dir().join("update.json")
}

/// What the title bar shows.
#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    /// Nothing newer (or not checked yet).
    UpToDate,
    Available,
    Downloading(f32),
    /// The installer is running; Snag is about to close.
    Installing,
    Failed(String),
}

/// What "Check for updates" (Settings > About) found this session.
#[derive(Clone, Debug, PartialEq)]
pub enum CheckStatus {
    NotChecked,
    Checking,
    UpToDate,
    Available(String),
    Failed(String),
}

enum Msg {
    Checked(Result<Release, String>),
    Downloaded(Result<PathBuf, String>),
}

/// The window's side: checks in the background and runs the update.
pub struct Updater {
    latest: Option<Release>,
    busy: Option<mpsc::Receiver<Msg>>,
    downloading: bool,
    installing: bool,
    error: Option<String>,
    /// A check is running.
    checking: bool,
    /// A check finished this session (Ok) or failed (Err).
    checked: Option<Result<(), String>>,
    progress: Arc<Progress>,
    next_check: Instant,
    /// Pretend to be this version (testing: `--pretend-version 0.5.0`).
    pretend: Option<String>,
}

impl Updater {
    pub fn new(pretend: Option<String>) -> Self {
        let cache: Cache = std::fs::read(cache_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let age = Duration::from_secs(crate::library::now().saturating_sub(cache.checked_at));
        let next_check = Instant::now() + first_check_in(age);
        Self {
            latest: cache.release,
            busy: None,
            downloading: false,
            installing: false,
            error: None,
            checking: false,
            checked: None,
            progress: Arc::default(),
            next_check,
            pretend,
        }
    }

    /// This build's version (or the pretended one when testing).
    pub fn version(&self) -> &str {
        self.pretend.as_deref().unwrap_or(current())
    }

    /// The release to update to, when there is a newer one.
    pub fn available(&self) -> Option<&Release> {
        self.latest.as_ref().filter(|r| is_newer(&r.version, self.version()))
    }

    pub fn phase(&self) -> Phase {
        if self.installing {
            Phase::Installing
        } else if self.downloading {
            Phase::Downloading(self.progress.fraction())
        } else if let Some(e) = &self.error {
            Phase::Failed(e.clone())
        } else if self.available().is_some() {
            Phase::Available
        } else {
            Phase::UpToDate
        }
    }

    /// Call every frame. `enabled`: the "check for updates" setting.
    pub fn poll(&mut self, enabled: bool) {
        if let Some(rx) = &self.busy {
            match rx.try_recv() {
                Ok(Msg::Checked(r)) => {
                    self.busy = None;
                    self.checking = false;
                    self.checked = Some(r.as_ref().map(|_| ()).map_err(Clone::clone));
                    self.next_check = Instant::now() + next_check_in(r.is_ok());
                    match r {
                        Ok(release) => {
                            let cache = Cache { checked_at: crate::library::now(), release: Some(release.clone()) };
                            if let Ok(json) = serde_json::to_vec_pretty(&cache) {
                                let _ = std::fs::write(cache_file(), json);
                            }
                            if Some(&release) != self.latest.as_ref() {
                                self.error = None;
                            }
                            log::info!("update check: latest release is {} (this is {})", release.version, self.version());
                            self.latest = Some(release);
                        }
                        // A failed check stays quiet: tried again later.
                        Err(e) => log::info!("update check failed: {e}"),
                    }
                }
                Ok(Msg::Downloaded(r)) => {
                    self.busy = None;
                    self.downloading = false;
                    match r.map_err(anyhow::Error::msg).and_then(|p| run_installer(&p).map(|_| p)) {
                        Ok(p) => {
                            log::info!("update: running {}", p.display());
                            self.installing = true;
                        }
                        Err(e) => {
                            log::warn!("update failed: {e:#}");
                            self.error = Some(format!("{e:#}"));
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.busy = None;
                    self.downloading = false;
                    self.checking = false;
                }
            }
            return;
        }
        if enabled && !self.installing && Instant::now() >= self.next_check {
            self.spawn_check();
        }
    }

    fn spawn_check(&mut self) {
        // Set properly when the answer arrives (poll).
        self.next_check = Instant::now() + CHECK_EVERY;
        self.checking = true;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Checked(fetch_latest().map_err(|e| format!("{e:#}"))));
        });
        self.busy = Some(rx);
    }

    /// "Check for updates": asks GitHub now (also when automatic checks
    /// are off).
    pub fn check_now(&mut self) {
        if self.busy.is_none() && !self.installing {
            self.checked = None;
            self.spawn_check();
        }
    }

    pub fn check_status(&self) -> CheckStatus {
        if self.checking {
            return CheckStatus::Checking;
        }
        match &self.checked {
            None => CheckStatus::NotChecked,
            Some(Err(e)) => CheckStatus::Failed(e.clone()),
            Some(Ok(())) => match self.available() {
                Some(r) => CheckStatus::Available(r.version.clone()),
                None => CheckStatus::UpToDate,
            },
        }
    }

    /// The title bar button was clicked.
    pub fn start(&mut self) {
        let Some(release) = self.available().cloned() else { return };
        if self.downloading || self.installing {
            return;
        }
        let asset = match release.installer {
            Some(a) if installed() => a,
            // Portable copy (or no installer in the release): the page.
            _ => {
                crate::win::shell_open(&release.page);
                return;
            }
        };
        // A check still running is simply dropped.
        self.checking = false;
        self.error = None;
        self.downloading = true;
        self.progress = Arc::default();
        let progress = self.progress.clone();
        let (tx, rx) = mpsc::channel();
        log::info!("update: downloading {}", asset.url);
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Downloaded(download(&asset, &progress).map_err(|e| format!("{e:#}"))));
        });
        self.busy = Some(rx);
    }

    /// Clicking opens the release page instead of installing.
    pub fn opens_page(&self) -> bool {
        !installed() || self.available().is_some_and(|r| r.installer.is_none())
    }

    pub fn page(&self) -> Option<&str> {
        self.available().map(|r| r.page.as_str())
    }

    pub fn busy(&self) -> bool {
        self.downloading || self.installing
    }

    /// Time until the next automatic check, so an idle window can ask to be
    /// woken for it. None while a check or the update itself is running.
    pub fn next_check_in(&self) -> Option<Duration> {
        (self.busy.is_none() && !self.installing).then(|| self.next_check.saturating_duration_since(Instant::now()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare() {
        assert_eq!(parse_version("v0.6.0"), Some((0, 6, 0)));
        assert_eq!(parse_version("1.2"), Some((1, 2, 0)));
        assert_eq!(parse_version("0.7.0-beta"), Some((0, 7, 0)));
        assert_eq!(parse_version("abc"), None);
        assert!(is_newer("0.6.1", "0.6.0"));
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(!is_newer("0.6.0", "0.6.0"));
        assert!(!is_newer("0.5.9", "0.6.0"));
    }

    #[test]
    fn reads_a_release() {
        let json = serde_json::json!({
            "tag_name": "v0.7.0",
            "html_url": "https://github.com/ezph4ntom/Snag/releases/tag/v0.7.0",
            "assets": [
                { "name": "Snag-v0.7.0.zip", "browser_download_url": "https://github.com/ezph4ntom/Snag/releases/download/v0.7.0/Snag-v0.7.0.zip", "size": 10 },
                { "name": "Snag-Setup-0.7.0.exe", "browser_download_url": "https://github.com/ezph4ntom/Snag/releases/download/v0.7.0/Snag-Setup-0.7.0.exe", "size": 42, "digest": "sha256:ABCDEF" }
            ]
        });
        let r = parse_release(&json).unwrap();
        assert_eq!(r.version, "0.7.0");
        let a = r.installer.unwrap();
        assert_eq!(a.name, "Snag-Setup-0.7.0.exe");
        assert_eq!(a.size, 42);
        assert_eq!(a.sha256.as_deref(), Some("abcdef"));

        // Files hosted anywhere else are never picked.
        let evil = serde_json::json!({
            "tag_name": "v9.0.0",
            "assets": [{ "name": "Snag-Setup-9.0.0.exe", "browser_download_url": "https://example.com/Snag-Setup-9.0.0.exe", "size": 1 }]
        });
        assert!(parse_release(&evil).unwrap().installer.is_none());
    }

    #[test]
    fn check_timing() {
        let min = |m: u64| Duration::from_secs(m * 60);
        // Window opened shortly after a check: reuse it, but not for long.
        assert_eq!(first_check_in(min(10)), min(20));
        // An older check (or none: age is "since 1970"): ask right away.
        assert_eq!(first_check_in(min(30)), Duration::ZERO);
        assert_eq!(first_check_in(min(5 * 60)), Duration::ZERO);
        assert_eq!(first_check_in(Duration::from_secs(crate::library::now())), Duration::ZERO);
        // Then hourly; a failed check is retried sooner.
        assert_eq!(next_check_in(true), min(60));
        assert_eq!(next_check_in(false), min(10));
    }

    #[test]
    fn sha256_matches_known_value() {
        assert_eq!(sha256_hex(b"abc").unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    /// Talks to GitHub: `cargo test -- --ignored latest_release`.
    #[test]
    #[ignore]
    fn latest_release() {
        let r = fetch_latest().unwrap();
        println!("{r:#?}");
        assert!(r.installer.is_some());
    }

    /// Downloads and checks the latest installer (doesn't run it):
    /// `cargo test -- --ignored downloads_the_installer`.
    #[test]
    #[ignore]
    fn downloads_the_installer() {
        let asset = fetch_latest().unwrap().installer.unwrap();
        let path = download(&asset, &Progress::default()).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), asset.size);
        println!("{} ok", path.display());
    }
}
