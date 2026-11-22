//! "What's new": the first time the window opens after an update, a short
//! list of what was added and fixed (for every version since the last one
//! the user saw, up to five). Settings > About opens it again.
//!
//! The notes are built in (they work offline and always match the version).
//! When releasing, add the new version's entry at the top of `HISTORY`: a
//! unit test fails until the current version has one.
//!
//! The last version seen is `settings.ui.last_seen_version`. A fresh install
//! records its own version there on first launch (the core does it), so the
//! dialog only ever shows after an update. Empty means "updated from a
//! version older than this feature": then only the current version's notes
//! are shown.

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

/// At most this many versions in one dialog.
const MAX_VERSIONS: usize = 5;

/// The notes to show for someone who last saw `last_seen` and now runs
/// `current`, newest first. Nothing when `last_seen` is `current` or newer;
/// only `current`'s notes when `last_seen` is empty or unreadable.
pub fn to_show(last_seen: &str, current: &str) -> Vec<&'static Notes> {
    use crate::update::{is_newer, parse_version};
    let Some(now) = parse_version(current) else { return Vec::new() };
    match parse_version(last_seen) {
        Some(seen) if seen >= now => Vec::new(),
        Some(_) => HISTORY
            .iter()
            .filter(|n| is_newer(n.version, last_seen) && !is_newer(n.version, current))
            .take(MAX_VERSIONS)
            .collect(),
        None => HISTORY.iter().filter(|n| parse_version(n.version) == Some(now)).collect(),
    }
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
        // Updated from before this feature (or unreadable): this version only.
        assert_eq!(versions(to_show("", "0.7.2")), ["0.7.2"]);
        assert_eq!(versions(to_show("garbage", "0.7.2")), ["0.7.2"]);
        // Never the future.
        assert_eq!(versions(to_show("0.6.0", "0.7.1")), ["0.7.1", "0.7.0"]);
    }
}
