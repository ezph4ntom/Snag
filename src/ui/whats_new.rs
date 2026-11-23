//! "What's new": the first time the window opens after an update, a short
//! list of what was added and fixed (for every version since the last one
//! the user saw: after skipping a few updates, all of them). Settings >
//! About opens it again.
//!
//! The notes are built in (they work offline and always match the version).
//! When releasing, add the new version's entry at the top of `HISTORY`: a
//! unit test fails until the current version has one.
//!
//! The last version seen is `settings.ui.last_seen_version`. A fresh install
//! records its own version there on first launch (the core does it), so the
//! dialog only ever shows after an update. Empty means "updated from 0.7.2
//! or older" (before this was recorded): then every version since 0.7.2.

use eframe::egui::{self, RichText, Sense, vec2};

use super::Shared;

/// One version's notes: short, plain sentences.
pub struct Notes {
    pub version: &'static str,
    pub new: &'static [&'static str],
    pub fixed: &'static [&'static str],
}

/// Newest first. The top entry must be the current version (tested).
pub const HISTORY: &[Notes] = &[
    Notes {
        version: "0.7.14",
        new: &[],
        fixed: &[
            "Updating no longer says ffmpeg is missing when it is installed.",
            "If ffmpeg is installed while Snag runs, recording starts by itself (no restart).",
        ],
    },
    Notes {
        version: "0.7.13",
        new: &[],
        fixed: &[
            "\"Only one app\": a game that is busy loading (like Fortnite) no longer turns the clip black.",
            "Saving a clip is about a third faster and writes half as much to disk.",
            "The replay buffer starts a little sooner.",
        ],
    },
    Notes {
        version: "0.7.12",
        new: &[
            "Clip editor: Ctrl+Z undoes a cut, Ctrl+Y redoes it (or the Undo / Redo buttons).",
            "Sound: turn a quiet mic or app up to 200 %.",
        ],
        fixed: &[
            "Recording works with the newest ffmpeg (9.1).",
            "The mouse wheel scrolls faster, like in other apps.",
            "Long clips: the editor's timeline labels no longer overlap.",
        ],
    },
    Notes {
        version: "0.7.11",
        new: &["Live preview: right-click it to pick Normal (30 fps) or Smooth (60 fps)."],
        fixed: &[
            "Uploads keep your clip's file name as their title.",
            "The live preview costs less when you make it small.",
        ],
    },
    Notes {
        version: "0.7.10",
        new: &[],
        fixed: &["Live tab: the status sits in a slim bar on top of the preview instead of the corner."],
    },
    Notes {
        version: "0.7.9",
        new: &["Right-click the live preview to hide it (the \"Hide preview\" link is gone)."],
        fixed: &["Skipped a few updates? This screen now shows everything since your old version."],
    },
    Notes {
        version: "0.7.8",
        new: &[],
        fixed: &["Live tab: Sources and Sound in clips line up with the preview."],
    },
    Notes {
        version: "0.7.7",
        new: &["Resize the live preview: drag its bottom-right corner (double-click for full size)."],
        fixed: &["The Live tab is tidier: recent clips moved out, and the preview and buttons scale with the window."],
    },
    Notes {
        version: "0.7.6",
        new: &[
            "New Live tab: big Save clip, Screenshot and Record tiles with their hotkeys.",
            "The replay buffer has an on/off switch next to its status.",
            "Recent clips show in a row under the buttons.",
        ],
        fixed: &[],
    },
    Notes {
        version: "0.7.5",
        new: &[
            "Clip editor: split, cut parts out, reorder, and join clips (Add clip).",
            "Save an edit as one clip, or each piece as its own file.",
        ],
        fixed: &[
            "The live preview has no black bars: it's the shape of what's recorded.",
            "Clips / Uploads: the count is centered.",
            "The editor uses much less CPU while playing a clip.",
        ],
    },
    Notes {
        version: "0.7.4",
        new: &[],
        fixed: &["Shorter notes here, and \"Got it\" is centered."],
    },
    Notes {
        version: "0.7.3",
        new: &["This \"What's new\" screen after each update (also in Settings > About)."],
        fixed: &[],
    },
    Notes {
        version: "0.7.2",
        new: &["Clips fill the frame: stretched resolutions (like FiveM 4:3) get no black bars."],
        fixed: &["\"Only one app\" no longer freezes when the game quits.", "The Live and Sound tabs use less CPU."],
    },
    Notes {
        version: "0.7.1",
        new: &[],
        fixed: &["The window fits at every size."],
    },
    Notes {
        version: "0.7.0",
        new: &["Sources: record only one app or game.", "Delete clips (to the Recycle Bin)."],
        fixed: &[],
    },
];

/// The last version without `last_seen_version` (added in 0.7.3): an
/// empty one means the update came from this version or older.
const BEFORE_TRACKING: &str = "0.7.2";

/// The notes to show for someone who last saw `last_seen` and now runs
/// `current`, newest first: every version since the one last seen (a jump
/// of several versions shows them all). Nothing when `last_seen` is
/// `current` or newer. Empty or unreadable: everything since 0.7.2, the
/// last version that didn't record it.
pub fn to_show(last_seen: &str, current: &str) -> Vec<&'static Notes> {
    use crate::update::parse_version;
    let Some(now) = parse_version(current) else { return Vec::new() };
    match parse_version(last_seen) {
        Some(seen) if seen >= now => Vec::new(),
        Some(_) => since(last_seen, current),
        None => {
            let all = since(BEFORE_TRACKING, current);
            if all.is_empty() { HISTORY.iter().filter(|n| parse_version(n.version) == Some(now)).collect() } else { all }
        }
    }
}

/// The versions after `seen`, up to `current`, newest first.
fn since(seen: &str, current: &str) -> Vec<&'static Notes> {
    use crate::update::is_newer;
    HISTORY.iter().filter(|n| is_newer(n.version, seen) && !is_newer(n.version, current)).collect()
}

/// The dialog. `open` holds the notes while it's shown.
#[derive(Default)]
pub struct WhatsNew {
    open: Vec<&'static Notes>,
}

impl WhatsNew {
    /// After an update: the notes since `last_seen` (nothing if none).
    pub fn after_update(last_seen: &str) -> Self {
        Self { open: to_show(last_seen, crate::update::current()) }
    }

    /// Settings > About: the latest versions' notes.
    pub fn show_recent(&mut self) {
        self.open = HISTORY.iter().take(3).collect();
    }

    /// Draws the dialog while it's open. Returns true when it was closed
    /// this frame (the caller records the version as seen).
    pub fn dialog(&mut self, ctx: &egui::Context, shared: &Shared) -> bool {
        if self.open.is_empty() {
            return false;
        }
        let c = shared.theme.colors.clone();
        let theme = &shared.theme;
        let mut close = false;
        let resp = egui::Modal::new(egui::Id::unique("whats-new")).show(ctx, |ui| {
            ui.set_width(480.0_f32.min(ctx.content_rect().width() - 80.0));
            let title = if self.open.len() == 1 { format!("What's new in Snag {}", self.open[0].version) } else { "What's new in Snag".to_string() };
            ui.label(RichText::new(title).family(crate::themes::brand_bold_family()).size(theme.fonts.body + 5.0));
            ui.add_space(8.0);
            let max_h = (ctx.content_rect().height() - 200.0).clamp(120.0, 420.0);
            egui::ScrollArea::vertical().max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
                let several = self.open.len() > 1;
                for (i, notes) in self.open.iter().enumerate() {
                    if several {
                        if i > 0 {
                            ui.add_space(10.0);
                        }
                        ui.label(RichText::new(format!("Snag {}", notes.version)).strong());
                        ui.add_space(2.0);
                    }
                    for (heading, items) in [("New", notes.new), ("Fixed", notes.fixed)] {
                        if items.is_empty() {
                            continue;
                        }
                        ui.label(RichText::new(heading).color(c.text_dim.0).small());
                        for item in items {
                            bullet(ui, c.accent.0, item);
                        }
                        ui.add_space(4.0);
                    }
                }
            });
            ui.add_space(12.0);
            // add_sized: a centered layout, so the text sits in the middle of
            // the button (egui aligns button text like the surrounding layout).
            let b = egui::Button::new(RichText::new("Got it").color(c.accent_text.0)).fill(c.accent.0);
            if ui.add_sized(vec2(90.0, 30.0), b).clicked() {
                close = true;
            }
        });
        if close || resp.should_close() {
            self.open.clear();
            return true;
        }
        false
    }
}

/// "• text", the text wrapping beside the dot.
fn bullet(ui: &mut egui::Ui, color: egui::Color32, text: &str) {
    ui.horizontal_top(|ui| {
        let line = ui.text_style_height(&egui::TextStyle::Body);
        let (dot, _) = ui.allocate_exact_size(vec2(14.0, line), Sense::hover());
        ui.painter().circle_filled(dot.center(), 2.5, color);
        ui.add(egui::Label::new(text).wrap());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn versions(v: Vec<&Notes>) -> Vec<&str> {
        v.into_iter().map(|n| n.version).collect()
    }

    #[test]
    fn the_current_version_has_notes() {
        // Releasing a new version? Add its notes at the top of HISTORY.
        assert_eq!(HISTORY[0].version, env!("CARGO_PKG_VERSION"), "HISTORY needs an entry for this version, first");
    }

    #[test]
    fn notes_are_newest_first_and_never_empty() {
        for pair in HISTORY.windows(2) {
            assert!(crate::update::is_newer(pair[0].version, pair[1].version), "{} before {}", pair[0].version, pair[1].version);
        }
        for n in HISTORY {
            assert!(!n.new.is_empty() || !n.fixed.is_empty(), "{} has no notes", n.version);
        }
    }

    #[test]
    fn shows_what_changed_since_the_last_version_seen() {
        assert_eq!(versions(to_show("0.7.0", "0.7.2")), ["0.7.2", "0.7.1"]);
        assert_eq!(versions(to_show("0.7.1", "0.7.2")), ["0.7.2"]);
        // Already seen, or a downgrade: nothing.
        assert!(to_show("0.7.2", "0.7.2").is_empty());
        assert!(to_show("0.9.0", "0.7.2").is_empty());
        // Several updates skipped: every one of them, however many.
        let all = ["0.7.9", "0.7.8", "0.7.7", "0.7.6", "0.7.5", "0.7.4", "0.7.3"];
        assert_eq!(versions(to_show("0.7.2", "0.7.9")), all);
        // Updated from 0.7.2 or older (nothing recorded yet), or unreadable:
        // everything since 0.7.2.
        assert_eq!(versions(to_show("", "0.7.9")), all);
        assert_eq!(versions(to_show("garbage", "0.7.9")), all);
        // ...and at 0.7.2 itself, just its own notes.
        assert_eq!(versions(to_show("", "0.7.2")), ["0.7.2"]);
        // A normal update: only the new version.
        assert_eq!(versions(to_show("0.7.8", "0.7.9")), ["0.7.9"]);
        // Two-digit patch numbers count as numbers (0.7.10 is after 0.7.9).
        assert_eq!(versions(to_show("0.7.9", "0.7.10")), ["0.7.10"]);
        assert!(crate::update::is_newer("0.7.10", "0.7.9"));
        // Never the future.
        assert_eq!(versions(to_show("0.6.0", "0.7.1")), ["0.7.1", "0.7.0"]);
    }
}
