//! The settings search in the title bar.
//!
//! Every settings section of every tab is listed here with the words people
//! are likely to type for it: its own labels plus everyday synonyms ("focus"
//! finds Capture, "boot" finds Startup, "mic" finds Sound sources). While a
//! search is typed, the window shows only the matching sections, fully
//! usable, instead of the current tab.

use super::settings_tab::SettingsSection;

/// A section that can be shown as a search result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Settings(SettingsSection),
    SoundRecording,
    SoundSources,
    QualityHardware,
    QualityPreset,
    QualityManual,
    Theme,
}

pub struct Entry {
    pub hit: Hit,
    /// "Settings", "Sound"...: shown above the result.
    pub tab: &'static str,
    pub title: &'static str,
    keywords: &'static str,
}

pub const ENTRIES: &[Entry] = &[
    Entry {
        hit: Hit::Settings(SettingsSection::Buffer),
        tab: "Settings",
        title: "Replay buffer",
        keywords: "clip length duration seconds minutes how long last replay buffer instant replay start buffer launch \
                   save time disk space",
    },
    Entry {
        hit: Hit::Settings(SettingsSection::Recording),
        tab: "Settings",
        title: "Recording",
        keywords: "record recording session whole full long gameplay start stop video capture mode save session automatic",
    },
    Entry {
        hit: Hit::Settings(SettingsSection::Hotkeys),
        tab: "Settings",
        title: "Hotkeys",
        keywords: "hotkey hotkeys key keys keybind keybinds bind binding shortcut shortcuts keyboard button buttons mouse \
                   save clip screenshot record ctrl alt shift f9 f10 f11 rebind change administrator admin not working",
    },
    Entry {
        hit: Hit::Settings(SettingsSection::Capture),
        tab: "Settings",
        title: "Capture",
        keywords: "capture monitor monitors screen screens display displays focus follow active window second dual multiple \
                   fixed cursor mouse pointer which screen record area",
    },
    Entry {
        hit: Hit::Settings(SettingsSection::Folders),
        tab: "Settings",
        title: "Folders",
        keywords: "folder folders path location directory where saved save location clips screenshots videos pictures \
                   storage drive disk find my clips",
    },
    Entry {
        hit: Hit::Settings(SettingsSection::ClipSound),
        tab: "Settings",
        title: "Clip sound",
        keywords: "sound beep notification chime alert ding clip saved wav mp3 custom volume mute quiet",
    },
    Entry {
        hit: Hit::Settings(SettingsSection::AfterClip),
        tab: "Settings",
        title: "After saving a clip",
        keywords: "after saving clip open trimmer minimized popup window focus game behaviour behavior default trim mode \
                   fast precise",
    },
    Entry {
        hit: Hit::Settings(SettingsSection::Startup),
        tab: "Settings",
        title: "Startup",
        keywords: "startup start with windows boot login sign in autostart auto start launch tray background run on startup \
                   update updates updater new version upgrade release github check latest",
    },
    Entry {
        hit: Hit::Settings(SettingsSection::Window),
        tab: "Settings",
        title: "Window",
        keywords: "window ui size scale zoom bigger smaller text font tab bar tabs dock position left right bottom top icons \
                   icon only live preview hide buffer time seconds minutes badge title bar format",
    },
    Entry {
        hit: Hit::Settings(SettingsSection::Uploads),
        tab: "Settings",
        title: "Uploads",
        keywords: "upload uploads streamable litterbox share sharing link links account login log in sign in password \
                   connect disconnect expiry expire delete",
    },
    Entry {
        hit: Hit::SoundRecording,
        tab: "Sound",
        title: "Sound recording",
        keywords: "sound audio record sound pc audio game audio quality bitrate aac kbps mute no sound",
    },
    Entry {
        hit: Hit::SoundSources,
        tab: "Sound",
        title: "Sound sources",
        keywords: "microphone mic voice discord spotify music apps app per app game audio output outputs device devices \
                   headset headphones speakers whole pc volume mixer levels sources chat",
    },
    Entry {
        hit: Hit::QualityHardware,
        tab: "Quality",
        title: "Detected hardware",
        keywords: "gpu graphics card video card encoder encoders nvenc nvidia amd intel quick sync cpu ram memory detect \
                   redetect hardware monitors",
    },
    Entry {
        hit: Hit::QualityPreset,
        tab: "Quality",
        title: "Quality preset",
        keywords: "preset quality balanced performance recommended auto tune lag fps drops smooth best",
    },
    Entry {
        hit: Hit::QualityManual,
        tab: "Quality",
        title: "Manual video settings",
        keywords: "resolution fps frame rate framerate bitrate encoder 1080p 1440p 720p 4k 60fps 120fps 144 manual \
                   override mbps file size video quality",
    },
    Entry {
        hit: Hit::Theme,
        tab: "Settings",
        title: "Theme",
        keywords: "theme themes appearance look colors colour colours dark mode light mode oled graphite midnight \
                   daylight warm ink black white",
    },
];

/// True when `a` and `b` differ by at most one edit: a missing, extra or
/// wrong letter, or two neighbouring letters swapped ("hotkye").
fn close(a: &str, b: &str) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.len().abs_diff(b.len()) > 1 {
        return false;
    }
    let (n, m) = (a.len(), b.len());
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j - 1] + cost).min(d[i - 1][j] + 1).min(d[i][j - 1] + 1);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[n][m] <= 1
}

/// How well one typed word matches a section (0 = not at all).
fn word_score(word: &str, title: &str, keywords: &str) -> u32 {
    let words = || title.split_whitespace().chain(keywords.split_whitespace());
    if title.split_whitespace().any(|t| t.starts_with(word)) {
        return 3;
    }
    if words().any(|t| t == word) {
        return 3;
    }
    if words().any(|t| t.starts_with(word)) {
        return 2;
    }
    if word.len() >= 4 && words().any(|t| close(t, word)) {
        return 1;
    }
    0
}

/// Sections matching `query`, best first. Every typed word must match
/// (filler words like "my", "the", "how" are ignored).
pub fn search(query: &str) -> Vec<&'static Entry> {
    const FILLER: &[&str] = &["my", "the", "a", "an", "to", "how", "do", "i", "of", "in", "on", "for", "is", "where", "are"];
    let q = query.to_lowercase();
    let words: Vec<&str> = q
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty() && !FILLER.contains(w))
        .collect();
    if words.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(u32, usize, &Entry)> = ENTRIES
        .iter()
        .enumerate()
        .filter_map(|(i, e)| {
            let title = e.title.to_lowercase();
            let mut total = 0;
            for w in &words {
                let s = word_score(w, &title, e.keywords);
                if s == 0 {
                    return None;
                }
                total += s;
            }
            Some((total, i, e))
        })
        .collect();
    scored.sort_by_key(|(score, i, _)| (std::cmp::Reverse(*score), *i));
    scored.into_iter().map(|(_, _, e)| e).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first(q: &str) -> Hit {
        search(q).first().unwrap_or_else(|| panic!("no result for {q:?}")).hit
    }

    #[test]
    fn everyday_words_find_the_right_section() {
        assert_eq!(first("focus"), Hit::Settings(SettingsSection::Capture));
        assert_eq!(first("second monitor"), Hit::Settings(SettingsSection::Capture));
        assert_eq!(first("boot"), Hit::Settings(SettingsSection::Startup));
        assert_eq!(first("start with windows"), Hit::Settings(SettingsSection::Startup));
        assert_eq!(first("keybind"), Hit::Settings(SettingsSection::Hotkeys));
        assert_eq!(first("mic"), Hit::SoundSources);
        assert_eq!(first("discord"), Hit::SoundSources);
        assert_eq!(first("dark mode"), Hit::Theme);
        assert_eq!(first("where are my clips"), Hit::Settings(SettingsSection::Folders));
        assert_eq!(first("clip length"), Hit::Settings(SettingsSection::Buffer));
        assert_eq!(first("streamable"), Hit::Settings(SettingsSection::Uploads));
        assert_eq!(first("1080p"), Hit::QualityManual);
        assert_eq!(first("gpu"), Hit::QualityHardware);
    }

    #[test]
    fn small_typos_are_forgiven() {
        assert_eq!(first("hotkye"), Hit::Settings(SettingsSection::Hotkeys));
        assert_eq!(first("microphon"), Hit::SoundSources);
    }

    #[test]
    fn nonsense_finds_nothing() {
        assert!(search("zzzz qqq").is_empty());
        assert!(search("   ").is_empty());
    }
}
