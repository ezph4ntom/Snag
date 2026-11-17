//! Uploading to Streamable the way its website does.
//!
//! Streamable's official API is read-only, so this follows the web upload
//! flow: sign in (session cookie), ask for an upload slot, register the
//! video, POST the file straight to Streamable's storage bucket, then ask
//! Streamable to process it. The password only ever goes to
//! `ajax.streamable.com` over HTTPS.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use ureq::Agent;

use super::http::{self, BROWSER_UA, Multipart, Progress};

const LOGIN_URL: &str = "https://ajax.streamable.com/check";
const API: &str = "https://api-f.streamable.com/api/v1";

/// What Streamable's free plan allows per video.
pub const FREE_MAX_BYTES: u64 = 250 * 1000 * 1000;
pub const FREE_MAX_SECONDS: f64 = 10.0 * 60.0;
/// Free-plan videos are removed after this many days.
pub const FREE_KEEP_DAYS: u64 = 90;

/// The signed-in account's plan.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Plan {
    /// "Free", "Pro"...
    pub name: String,
    /// Paid plans keep videos and allow longer/bigger ones.
    pub free: bool,
    /// Largest video the plan accepts.
    pub max_bytes: u64,
}

/// Raised when Streamable says the session isn't signed in.
#[derive(Debug)]
pub struct NotSignedIn;

impl std::fmt::Display for NotSignedIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Streamable signed us out")
    }
}

impl std::error::Error for NotSignedIn {}

pub struct Client {
    agent: Agent,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub fn new() -> Client {
        Client { agent: http::agent(BROWSER_UA) }
    }

    fn post_json(&self, url: &str, body: &Value) -> Result<(u16, String)> {
        let mut resp = self
            .agent
            .post(url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("Origin", "https://streamable.com")
            .header("Referer", "https://streamable.com/")
            .send(serde_json::to_vec(body)?)
            .with_context(|| format!("contacting {}", http::host_of(url)))?;
        let status = resp.status().as_u16();
        Ok((status, resp.body_mut().read_to_string().unwrap_or_default()))
    }

    /// Signs in; returns the account's plan.
    pub fn login(&self, email: &str, password: &str) -> Result<Plan> {
        let (status, text) = self.post_json(LOGIN_URL, &json!({ "username": email, "password": password }))?;
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let message = v.get("message").and_then(Value::as_str).or_else(|| v.get("error").and_then(Value::as_str));
        if !(200..300).contains(&status) || v.get("error").is_some_and(|e| !e.is_null() && e != false) {
            let why = message.map(str::to_string).unwrap_or_else(|| format!("HTTP {status}"));
            bail!("Streamable didn't accept the login: {why}");
        }
        if v.is_null() {
            bail!("Streamable answered the login strangely (HTTP {status}): {}", http::snippet(&text));
        }
        let name = v.get("plan_name").and_then(Value::as_str).unwrap_or("Free").to_string();
        let free = name.is_empty() || name.to_lowercase().contains("free") || name.to_lowercase() == "basic";
        let max_gb = v.get("plan_max_size").and_then(Value::as_f64).unwrap_or(0.0);
        let max_bytes = if free || max_gb <= 0.0 { FREE_MAX_BYTES } else { (max_gb * 1e9) as u64 };
        Ok(Plan { name: if name.is_empty() { "Free".into() } else { name }, free, max_bytes })
    }

    /// Uploads `file`; returns the video's shortcode (link:
    /// `https://streamable.com/<shortcode>`).
    pub fn upload(&self, file: &Path, title: &str, progress: &Arc<Progress>) -> Result<String> {
        let size = std::fs::metadata(file)?.len();
        let name = file.file_name().unwrap_or_default().to_string_lossy().to_string();

        // 1. An upload slot: shortcode + where to put the file.
        let mut resp = self
            .agent
            .get(format!("{API}/uploads/shortcode?size={size}&version=unknown"))
            .header("Accept", "application/json")
            .header("Origin", "https://streamable.com")
            .header("Referer", "https://streamable.com/")
            .call()
            .context("contacting Streamable")?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        if status == 401 || status == 403 {
            return Err(anyhow!(NotSignedIn));
        }
        if !(200..300).contains(&status) {
            bail!("Streamable refused the upload (HTTP {status}): {}", http::snippet(&text));
        }
        let meta: Value = serde_json::from_str(&text).context("Streamable sent an unexpected upload slot")?;
        let shortcode = meta.get("shortcode").and_then(Value::as_str).context("no shortcode in Streamable's answer")?.to_string();
        if progress.cancelled() {
            bail!("cancelled");
        }

        // 2. Register the video.
        let (status, text) = self.post_json(
            &format!("{API}/videos/{shortcode}/initialize"),
            &json!({ "original_size": size, "original_name": name, "upload_source": "web", "title": title }),
        )?;
        if !(200..300).contains(&status) {
            bail!("Streamable couldn't register the video (HTTP {status}): {}", http::snippet(&text));
        }

        // 3. The file itself, as a form POST to the storage bucket.
        let url = meta.get("url").and_then(Value::as_str);
        let fields = meta.get("fields").and_then(Value::as_object);
        let (Some(url), Some(fields)) = (url, fields) else {
            let keys: Vec<&str> = meta.as_object().map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default();
            bail!("Streamable changed how uploads work (got: {}); Snag needs an update", keys.join(", "));
        };
        let mut form = Multipart::new();
        for (k, v) in fields {
            let v = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            form = form.text(k, &v);
        }
        // Storage gets no Streamable cookies or browser headers.
        let plain = http::agent(BROWSER_UA);
        let (status, text) = form.post_file(&plain, url, "file", file, "video/mp4", progress)?;
        if !(200..300).contains(&status) {
            bail!("the upload to Streamable's storage failed (HTTP {status}): {}", http::snippet(&text));
        }

        // 4. Start processing (the link works from now on).
        let mut body = json!({ "upload_source": "web", "title": title });
        if let (Some(obj), Some(opts)) = (body.as_object_mut(), meta.get("transcoder_options").and_then(Value::as_object)) {
            for (k, v) in opts {
                obj.insert(k.clone(), v.clone());
            }
        }
        let (status, text) = self.post_json(&format!("{API}/transcode/{shortcode}"), &body)?;
        if !(200..300).contains(&status) {
            bail!("Streamable couldn't start processing the video (HTTP {status}): {}", http::snippet(&text));
        }
        Ok(shortcode)
    }
}

pub fn link(shortcode: &str) -> String {
    format!("https://streamable.com/{shortcode}")
}
