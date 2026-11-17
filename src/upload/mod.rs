//! Sharing clips as links: Streamable (with the user's account) or Litterbox
//! (anonymous, temporary). Uploads run on their own threads; the window
//! polls [`Uploader`] for progress and results.
//!
//! Routing: signed in to Streamable and the clip fits the plan -> Streamable.
//! Not signed in, or bigger/longer than the plan allows (free: 250 MB,
//! 10 min) -> Litterbox. Over 1 GB -> refused ("trim it first").

pub mod credentials;
pub mod http;
pub mod litterbox;
pub mod streamable;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};

use crate::library::{Host, UploadRecord, now};
use http::Progress;
use streamable::Plan;

/// What to upload.
pub struct Request {
    pub path: PathBuf,
    pub title: String,
    /// Length, if already known (else probed).
    pub seconds: Option<f64>,
    /// The connected Streamable plan (None: not connected).
    pub plan: Option<Plan>,
    /// Force a host (the "Use Litterbox" button after a Streamable failure).
    pub force: Option<Host>,
    pub litterbox_hours: u32,
}

#[derive(Clone, Debug)]
pub struct Failure {
    pub error: String,
    pub host: Option<Host>,
    /// Litterbox could take it instead.
    pub can_litterbox: bool,
}

/// An upload as the window shows it.
pub enum View<'a> {
    /// `host` is None while it's being decided; `fraction` 0..=1.
    Busy { host: Option<Host>, fraction: f32 },
    Failed(&'a Failure),
}

struct Job {
    progress: Arc<Progress>,
    host: Arc<Mutex<Option<Host>>>,
    rx: Receiver<Result<UploadRecord, Failure>>,
}

#[derive(Default)]
pub struct Uploader {
    jobs: HashMap<PathBuf, Job>,
    failed: HashMap<PathBuf, Failure>,
    login: Option<Receiver<Result<(String, Plan), String>>>,
}

/// Where a clip goes (see the module docs).
pub fn choose_host(bytes: u64, seconds: Option<f64>, plan: Option<&Plan>) -> Result<Host, String> {
    let fits_streamable =
        plan.is_some_and(|p| bytes <= p.max_bytes && !(p.free && seconds.is_some_and(|s| s > streamable::FREE_MAX_SECONDS)));
    if fits_streamable {
        Ok(Host::Streamable)
    } else if bytes <= litterbox::MAX_BYTES {
        Ok(Host::Litterbox)
    } else {
        Err("Over 1 GB, too big to upload. Trim it first.".into())
    }
}

fn probe_seconds(path: &Path) -> Option<f64> {
    let out = crate::win::hidden_command(crate::paths::ffprobe_exe())
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

impl Uploader {
    pub fn start(&mut self, req: Request) {
        if self.jobs.contains_key(&req.path) {
            return;
        }
        self.failed.remove(&req.path);
        let progress = Arc::new(Progress::default());
        let host = Arc::new(Mutex::new(None));
        let (tx, rx) = channel();
        let (p, h, path) = (progress.clone(), host.clone(), req.path.clone());
        let key = path.clone();
        std::thread::Builder::new()
            .name("upload".into())
            .spawn(move || {
                let result = run(req, &p, &h);
                if let Err(f) = &result {
                    log::warn!("upload of {} failed: {}", key.display(), f.error);
                }
                let _ = tx.send(result);
            })
            .ok();
        self.jobs.insert(path, Job { progress, host, rx });
    }

    pub fn view(&self, path: &Path) -> Option<View<'_>> {
        if let Some(j) = self.jobs.get(path) {
            let host = *j.host.lock().unwrap_or_else(|e| e.into_inner());
            return Some(View::Busy { host, fraction: j.progress.fraction() });
        }
        self.failed.get(path).map(View::Failed)
    }

    /// Finished uploads since the last call (failures stay in `view`).
    pub fn poll(&mut self) -> Vec<UploadRecord> {
        let mut done = Vec::new();
        let mut finished = Vec::new();
        for (path, j) in &self.jobs {
            match j.rx.try_recv() {
                Ok(Ok(rec)) => {
                    done.push(rec);
                    finished.push(path.clone());
                }
                Ok(Err(f)) => {
                    self.failed.insert(path.clone(), f);
                    finished.push(path.clone());
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.failed.insert(
                        path.clone(),
                        Failure { error: "the upload stopped unexpectedly".into(), host: None, can_litterbox: true },
                    );
                    finished.push(path.clone());
                }
            }
        }
        for p in finished {
            self.jobs.remove(&p);
        }
        done
    }

    /// Uploads still running.
    pub fn busy(&self) -> usize {
        self.jobs.len()
    }

    pub fn cancel(&mut self, path: &Path) {
        if let Some(j) = self.jobs.get(path) {
            j.progress.cancel.store(true, Ordering::Relaxed);
        }
    }

    pub fn cancel_all(&mut self) {
        for j in self.jobs.values() {
            j.progress.cancel.store(true, Ordering::Relaxed);
        }
    }

    pub fn dismiss(&mut self, path: &Path) {
        self.failed.remove(path);
    }

    // ---- Streamable login --------------------------------------------------

    /// Checks the login with Streamable on a worker thread; saved to
    /// Credential Manager only if Streamable accepts it.
    pub fn login(&mut self, email: String, password: String) {
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let result = streamable::Client::new()
                .login(&email, &password)
                .and_then(|plan| credentials::save(&email, &password).map(|()| plan))
                .map(|plan| (email, plan))
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
        self.login = Some(rx);
    }

    pub fn logging_in(&self) -> bool {
        self.login.is_some()
    }

    /// The login's result, once: Ok((email, plan)) or the error.
    pub fn poll_login(&mut self) -> Option<Result<(String, Plan), String>> {
        let r = self.login.as_ref()?.try_recv().ok()?;
        self.login = None;
        Some(r)
    }
}

fn run(req: Request, progress: &Arc<Progress>, host_slot: &Mutex<Option<Host>>) -> Result<UploadRecord, Failure> {
    let fail = |error: String, host: Option<Host>, can_litterbox: bool| Failure { error, host, can_litterbox };
    let bytes = std::fs::metadata(&req.path).map(|m| m.len()).map_err(|e| fail(format!("can't read the clip: {e}"), None, false))?;
    let host = match req.force {
        Some(h) => h,
        None => {
            let seconds = req.seconds.or_else(|| probe_seconds(&req.path));
            choose_host(bytes, seconds, req.plan.as_ref()).map_err(|e| fail(e, None, false))?
        }
    };
    *host_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(host);
    let can_litterbox = bytes <= litterbox::MAX_BYTES;
    let uploaded_at = now();
    let (url, expires_at) = match host {
        Host::Litterbox => {
            let url = litterbox::upload(&req.path, req.litterbox_hours, progress)
                .map_err(|e| fail(format!("{e:#}"), Some(host), false))?;
            (url, Some(uploaded_at + req.litterbox_hours.max(1) as u64 * 3600))
        }
        Host::Streamable => {
            let sc = streamable_upload(&req, progress).map_err(|e| fail(format!("{e:#}"), Some(host), can_litterbox))?;
            let free = req.plan.as_ref().is_none_or(|p| p.free);
            (streamable::link(&sc), free.then_some(uploaded_at + streamable::FREE_KEEP_DAYS * 86_400))
        }
    };
    log::info!("uploaded {} to {}", req.path.display(), host.name());
    Ok(UploadRecord { path: req.path.clone(), title: req.title.clone(), host, url, uploaded_at, expires_at, bytes })
}

fn streamable_upload(req: &Request, progress: &Arc<Progress>) -> anyhow::Result<String> {
    let login = credentials::load().ok_or_else(|| anyhow::anyhow!("not signed in to Streamable (Settings > Uploads)"))?;
    let client = streamable::Client::new();
    client.login(&login.email, &login.password)?;
    match client.upload(&req.path, &req.title, progress) {
        Err(e) if e.is::<streamable::NotSignedIn>() => {
            client.login(&login.email, &login.password)?;
            client.upload(&req.path, &req.title, progress)
        }
        r => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn free() -> Plan {
        Plan { name: "Free".into(), free: true, max_bytes: streamable::FREE_MAX_BYTES }
    }

    #[test]
    fn routing() {
        const MB: u64 = 1_000_000;
        assert_eq!(choose_host(50 * MB, Some(30.0), None), Ok(Host::Litterbox), "not signed in");
        assert_eq!(choose_host(50 * MB, Some(30.0), Some(&free())), Ok(Host::Streamable));
        assert_eq!(choose_host(300 * MB, Some(30.0), Some(&free())), Ok(Host::Litterbox), "too big for free");
        assert_eq!(choose_host(50 * MB, Some(700.0), Some(&free())), Ok(Host::Litterbox), "too long for free");
        let pro = Plan { name: "Pro".into(), free: false, max_bytes: 10_000 * MB };
        assert_eq!(choose_host(1_200 * MB, Some(1800.0), Some(&pro)), Ok(Host::Streamable), "paid plans take big files");
        assert!(choose_host(1_200 * MB, None, Some(&free())).is_err(), "over 1 GB");
        assert!(choose_host(1_200 * MB, None, None).is_err(), "over 1 GB");
    }
}
