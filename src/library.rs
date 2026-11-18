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
    /// How the Clips tab lays clips out.
    pub view: ClipView,
}

/// The Clips tab's layouts.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClipView {
    /// Cards with a big thumbnail, upload and trim buttons.
    #[default]
    Grid,
    /// Smaller cards: thumbnail, title and length.
    Compact,
    /// One row per clip.
    List,
}

/// Checks that `path` is something the Clips tab may move to the Recycle
/// Bin: an existing .mp4 or .png file inside `clips_dir` (after resolving
/// `..` and links). Returns the resolved path.
pub fn deletable(path: &Path, clips_dir: &Path) -> Result<PathBuf, String> {
    let ok_ext = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp4") || e.eq_ignore_ascii_case("png"));
    if !ok_ext {
        return Err("only .mp4 and .png files can be deleted here".into());
    }
    let file = std::fs::canonicalize(path).map_err(|_| "the file is gone".to_string())?;
    let dir = std::fs::canonicalize(clips_dir).map_err(|_| "the clips folder is missing".to_string())?;
    if !file.is_file() || !file.starts_with(&dir) || file == dir {
        return Err("only clips in Snag's clips folder can be deleted here".into());
    }
    Ok(plain_path(file))
}

/// `\\?\C:\x` -> `C:\x` and `\\?\UNC\server\share` -> `\\server\share`:
/// canonicalize's long-path form, which the Recycle Bin call refuses.
fn plain_path(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        p
    }
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

#[cfg(test)]
mod tests {
    use super::deletable;

    #[test]
    fn only_clips_in_the_clips_folder_can_be_deleted() {
        let root = std::env::temp_dir().join(format!("snag-delete-guard-{}", std::process::id()));
        let clips = root.join("clips");
        let other = root.join("other");
        std::fs::create_dir_all(&clips).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        for f in [clips.join("a.mp4"), clips.join("b.PNG"), clips.join("c.txt"), other.join("d.mp4")] {
            std::fs::write(f, b"x").unwrap();
        }
        std::fs::create_dir_all(clips.join("folder.mp4")).unwrap();

        let ok = deletable(&clips.join("a.mp4"), &clips).unwrap();
        assert!(!ok.to_string_lossy().starts_with(r"\\?\"), "plain path for the Recycle Bin: {}", ok.display());
        assert!(ok.ends_with("a.mp4") && ok.is_file());
        assert_eq!(super::plain_path(r"\\?\UNC\nas\share\c.mp4".into()), std::path::PathBuf::from(r"\\nas\share\c.mp4"));
        assert!(deletable(&clips.join("b.PNG"), &clips).is_ok());
        // Wrong type, outside the folder (also via ".."), missing, a folder.
        assert!(deletable(&clips.join("c.txt"), &clips).is_err());
        assert!(deletable(&other.join("d.mp4"), &clips).is_err());
        assert!(deletable(&clips.join("..").join("other").join("d.mp4"), &clips).is_err());
        assert!(deletable(&clips.join("gone.mp4"), &clips).is_err());
        assert!(deletable(&clips.join("folder.mp4"), &clips).is_err());
        // A clips folder named like the start of another folder.
        let clips2 = root.join("clips2");
        std::fs::create_dir_all(&clips2).unwrap();
        std::fs::write(clips2.join("e.mp4"), b"x").unwrap();
        assert!(deletable(&clips2.join("e.mp4"), &clips).is_err());

        let _ = std::fs::remove_dir_all(&root);
    }
}
