//! What the clip library remembers besides the files themselves:
//! favourites, imported folders ("collections"), uploaded links and the
//! Streamable plan. `%APPDATA%\Snag\library.json`, written only by the
//! window process.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::upload::streamable::Plan;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Host {
    Streamable,
    Litterbox,
}

impl Host {
    pub fn name(self) -> &'static str {
        match self {
            Host::Streamable => "Streamable",
            Host::Litterbox => "Litterbox",
        }
    }
}

/// A clip that was uploaded, and where.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct UploadRecord {
    pub path: PathBuf,
    pub title: String,
    pub host: Host,
    pub url: String,
    /// Unix seconds.
    pub uploaded_at: u64,
    /// Unix seconds; None = kept until deleted.
    pub expires_at: Option<u64>,
    pub bytes: u64,
}

impl UploadRecord {
    pub fn expired(&self) -> bool {
        self.expires_at.is_some_and(|e| e <= now())
    }

    /// "Expires in 3 days" / "Expired" / "Kept until you delete it".
    pub fn expiry_text(&self) -> String {
        let Some(at) = self.expires_at else { return "Never expires".into() };
        let left = at.saturating_sub(now());
        match left {
            0 => "Expired".into(),
            1..=3599 => format!("Expires in {} min", left.div_ceil(60)),
            3600..=259_199 => format!("Expires in {} h", left / 3600),
            _ => format!("Expires in {} days", left / 86_400),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Library {
    pub favorites: BTreeSet<PathBuf>,
    /// Extra folders shown in the library, each as its own filter.
    pub collections: Vec<PathBuf>,
    /// Newest last.
    pub uploads: Vec<UploadRecord>,
    /// The "Log in with Streamable / Skip" card was answered.
    pub onboarding_done: bool,
    /// The connected Streamable account's plan (None = not connected).
    pub streamable: Option<Plan>,
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Library {
    pub fn file() -> PathBuf {
        crate::paths::config_dir().join("library.json")
    }

    pub fn load() -> Library {
        std::fs::read_to_string(Self::file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    /// Writes atomically (temp file + rename).
    pub fn save(&self) {
        let path = Self::file();
        let tmp = path.with_extension("json.tmp");
        let result = std::fs::create_dir_all(crate::paths::config_dir())
            .and_then(|()| std::fs::write(&tmp, serde_json::to_vec_pretty(self).unwrap_or_default()))
            .and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(e) = result {
            log::warn!("couldn't save the library: {e}");
        }
    }

    /// The live (unexpired) upload of this file, newest first.
    pub fn upload_of(&self, path: &Path) -> Option<&UploadRecord> {
        self.uploads.iter().rev().find(|u| u.path == path && !u.expired())
    }
}
