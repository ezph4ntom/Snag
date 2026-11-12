//! JSON themes from the `themes/` folder. See `themes/README.md` for the
//! schema. Every field is optional; anything missing falls back to the
//! built-in Daylight theme, so a theme file can be as small as
//! `{"colors":{"accent":"#ff0080"}}`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use eframe::egui::{self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Stroke, TextStyle, Vec2};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A color written as `"#RRGGBB"` or `"#RRGGBBAA"` in JSON.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hex(pub Color32);

impl Hex {
    const fn rgb(r: u8, g: u8, b: u8) -> Hex {
        Hex(Color32::from_rgb(r, g, b))
    }
}

impl Serialize for Hex {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let [r, g, b, a] = self.0.to_srgba_unmultiplied();
        let text = if a == 255 { format!("#{r:02x}{g:02x}{b:02x}") } else { format!("#{r:02x}{g:02x}{b:02x}{a:02x}") };
        s.serialize_str(&text)
    }
}

impl<'de> Deserialize<'de> for Hex {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Hex, D::Error> {
        let text = String::deserialize(d)?;
        parse_hex(&text).map(Hex).ok_or_else(|| serde::de::Error::custom(format!("bad color \"{text}\" (use #RRGGBB or #RRGGBBAA)")))
    }
}

fn parse_hex(text: &str) -> Option<Color32> {
    let h = text.trim().strip_prefix('#')?;
    let byte = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok();
    match h.len() {
        6 => Some(Color32::from_rgb(byte(0)?, byte(2)?, byte(4)?)),
        8 => Some(Color32::from_rgba_unmultiplied(byte(0)?, byte(2)?, byte(4)?, byte(6)?)),
        _ => None,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Colors {
    /// Window background.
    pub background: Hex,
    /// Side panels, cards, the status bar.
    pub panel: Hex,
    pub text: Hex,
    /// Secondary text (hints, labels).
    pub text_dim: Hex,
    /// Highlights: selected tab, primary buttons, slider fill, links.
    pub accent: Hex,
    /// Text drawn on top of `accent`.
    pub accent_text: Hex,
    /// Buttons, text boxes, combo boxes.
    pub widget: Hex,
    pub widget_hover: Hex,
    pub widget_active: Hex,
    pub border: Hex,
    pub success: Hex,
    pub warning: Hex,
    pub error: Hex,
    /// Trimmer timeline track.
    pub timeline: Hex,
    /// The kept region between the trim handles.
    pub timeline_selection: Hex,
    pub playhead: Hex,
    /// Small controls: slider rails, checkbox/radio fills, scrollbar
    /// handles. Optional; defaults to `widget`. Needed when `widget` is the
    /// same as the background (e.g. pure black), or those parts vanish.
    pub control: Option<Hex>,
    /// Behind the live preview and clip thumbnails (dark in every theme, so
    /// the picture's colors read true).
    pub preview: Hex,
}

impl Default for Colors {
    /// Daylight: warm off-white, white cards, ink text, Ember-red accent.
    fn default() -> Self {
        Colors {
            background: Hex::rgb(0xf4, 0xf3, 0xf0),
            panel: Hex::rgb(0xff, 0xff, 0xff),
            text: Hex::rgb(0x1b, 0x1c, 0x1f),
            text_dim: Hex::rgb(0x5d, 0x60, 0x68),
            accent: Hex::rgb(0xd9, 0x44, 0x1f),
            accent_text: Hex::rgb(0xff, 0xff, 0xff),
            widget: Hex::rgb(0xff, 0xff, 0xff),
            widget_hover: Hex::rgb(0xf2, 0xf1, 0xed),
            widget_active: Hex::rgb(0xe8, 0xe6, 0xe1),
            border: Hex::rgb(0xe2, 0xe0, 0xda),
            success: Hex::rgb(0x1f, 0x8a, 0x5b),
            warning: Hex::rgb(0xb7, 0x79, 0x1f),
            error: Hex::rgb(0xc6, 0x28, 0x28),
            timeline: Hex::rgb(0xe8, 0xe6, 0xe1),
            timeline_selection: Hex(Color32::from_rgba_unmultiplied(0xd9, 0x44, 0x1f, 0x40)),
            playhead: Hex::rgb(0x1b, 0x1c, 0x1f),
            control: Some(Hex::rgb(0xe8, 0xe6, 0xe1)),
            preview: Hex::rgb(0x1b, 0x1c, 0x1f),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Fonts {
    /// Sizes in points.
    pub body: f32,
    pub heading: f32,
    pub button: f32,
    pub small: f32,
    pub mono: f32,
    /// Optional .ttf/.otf file, relative to the `themes/` folder. `null`
    /// keeps egui's built-in font.
    pub family: Option<String>,
}

impl Default for Fonts {
    fn default() -> Self {
        Fonts {
            body: 14.0,
            heading: 20.0,
            button: 14.0,
            small: 12.0,
            mono: 13.0,
            family: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Spacing {
    /// Gap between widgets `[x, y]`.
    pub item: [f32; 2],
    /// Padding inside buttons `[x, y]`.
    pub button_padding: [f32; 2],
    /// Inner margin of windows and panels.
    pub window_margin: f32,
    pub indent: f32,
}

impl Default for Spacing {
    fn default() -> Self {
        Spacing {
            item: [8.0, 6.0],
            button_padding: [14.0, 7.0],
            window_margin: 12.0,
            indent: 18.0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Theme {
    pub name: String,
    /// Base for anything not covered here (scrollbars, shadows...).
    pub dark: bool,
    pub colors: Colors,
    pub fonts: Fonts,
    /// Corner radius of widgets, in points (windows get 1.5x).
    pub corner_radius: f32,
    pub spacing: Spacing,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            name: "Daylight (built-in)".into(),
            dark: false,
            colors: Colors::default(),
            fonts: Fonts::default(),
            corner_radius: 8.0,
            spacing: Spacing::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ThemeEntry {
    /// File stem, stored in settings.
    pub id: String,
    pub name: String,
    pub path: PathBuf,
}

/// All `*.json` files in the themes folder, sorted by name.
pub fn list() -> Vec<ThemeEntry> {
    let dir = crate::paths::themes_dir();
    let mut out: Vec<ThemeEntry> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("json")))
        .map(|path| {
            let id = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
            let name = load_file(&path).map(|t| t.name).unwrap_or_else(|_| format!("{id} (invalid)"));
            ThemeEntry { id, name, path }
        })
        .collect();
    out.sort_by_key(|t| t.name.to_lowercase());
    out
}

pub fn path_of(id: &str) -> PathBuf {
    crate::paths::themes_dir().join(format!("{id}.json"))
}

pub fn load_file(path: &Path) -> Result<Theme> {
    let text = crate::config::read_json_text(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// The theme new users get (and anyone whose theme file is gone).
pub const DEFAULT_ID: &str = "daylight";

/// `id` if that theme exists, else the default one (themes were renamed in
/// v0.5: "dark", "tide"... are gone).
pub fn existing_or_default(id: &str) -> String {
    if path_of(id).is_file() { id.to_string() } else { DEFAULT_ID.to_string() }
}

/// Loads a theme by id, falling back to the built-in theme on error.
pub fn load(id: &str) -> (Theme, Option<String>) {
    match load_file(&path_of(id)) {
        Ok(t) => (t, None),
        Err(e) => {
            log::warn!("theme {id}: {e:#}");
            (Theme::default(), Some(format!("{e:#}")))
        }
    }
}

fn radius(r: f32) -> CornerRadius {
    CornerRadius::same(r.clamp(0.0, 255.0) as u8)
}

/// Applies `theme` to the egui context. `fonts_changed`: also reload the
/// font family (slower, so only when it actually changed).
pub fn apply(ctx: &egui::Context, theme: &Theme, fonts_changed: bool) {
    let c = &theme.colors;
    let mut visuals = if theme.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    visuals.override_text_color = Some(c.text.0);
    visuals.weak_text_color = Some(c.text_dim.0);
    visuals.panel_fill = c.panel.0;
    visuals.window_fill = c.background.0;
    visuals.faint_bg_color = c.panel.0;
    visuals.extreme_bg_color = c.background.0;
    visuals.code_bg_color = c.widget.0;
    visuals.window_stroke = Stroke::new(1.0, c.border.0);
    visuals.hyperlink_color = c.accent.0;
    visuals.warn_fg_color = c.warning.0;
    visuals.error_fg_color = c.error.0;
    visuals.selection.bg_fill = c.accent.0;
    visuals.selection.stroke = Stroke::new(1.0, c.accent_text.0);
    visuals.window_corner_radius = radius(theme.corner_radius * 1.5);
    visuals.menu_corner_radius = radius(theme.corner_radius);

    // egui draws buttons, combo boxes and selectable rows with `weak_bg_fill`,
    // and slider rails, checkbox/radio boxes and scrollbar handles with
    // `bg_fill`; the optional `control` color goes to the latter.
    let control = c.control.map(|h| h.0);
    let w = &mut visuals.widgets;
    for (wv, fill) in [
        (&mut w.noninteractive, c.panel.0),
        (&mut w.inactive, c.widget.0),
        (&mut w.hovered, c.widget_hover.0),
        (&mut w.active, c.widget_active.0),
        (&mut w.open, c.widget_active.0),
    ] {
        wv.bg_fill = fill;
        wv.weak_bg_fill = fill;
        wv.corner_radius = radius(theme.corner_radius);
        wv.fg_stroke = Stroke::new(1.0, c.text.0);
    }
    if let Some(ctrl) = control {
        for wv in [&mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
            wv.bg_fill = ctrl;
        }
    }
    w.noninteractive.bg_stroke = Stroke::new(1.0, c.border.0);
    w.noninteractive.fg_stroke = Stroke::new(1.0, c.text.0);
    w.inactive.bg_stroke = Stroke::new(1.0, c.border.0);
    w.hovered.bg_stroke = Stroke::new(1.0, c.accent.0);
    w.active.bg_stroke = Stroke::new(1.0, c.accent.0);

    let mut style = (*ctx.global_style()).clone();
    style.visuals = visuals;
    let f = &theme.fonts;
    let family = FontFamily::Proportional;
    // Headings use the brand's bold weight, unless the theme brings its own
    // (single-weight) font.
    let heading_family = if f.family.is_some() { family.clone() } else { brand_bold_family() };
    style.text_styles = [
        (TextStyle::Heading, FontId::new(f.heading, heading_family)),
        (TextStyle::Body, FontId::new(f.body, family.clone())),
        (TextStyle::Button, FontId::new(f.button, family.clone())),
        (TextStyle::Small, FontId::new(f.small, family)),
        (TextStyle::Monospace, FontId::new(f.mono, FontFamily::Monospace)),
    ]
    .into();
    let s = &theme.spacing;
    style.spacing.item_spacing = Vec2::new(s.item[0], s.item[1]);
    style.spacing.button_padding = Vec2::new(s.button_padding[0], s.button_padding[1]);
    style.spacing.window_margin = Margin::same(s.window_margin.clamp(0.0, 127.0) as i8);
    style.spacing.indent = s.indent;
    // No built-in fade animations: each repaints the whole window at the
    // monitor refresh rate (see ui::run).
    style.animation_time = 0.0;

    let egui_theme = if theme.dark { egui::Theme::Dark } else { egui::Theme::Light };
    ctx.set_theme(egui_theme);
    ctx.set_style_of(egui_theme, style);

    if fonts_changed {
        let mut defs = brand_fonts();
        if let Some(file) = &f.family {
            let path = crate::paths::themes_dir().join(file);
            match std::fs::read(&path) {
                Ok(bytes) => {
                    defs.font_data.insert("theme".into(), Arc::new(FontData::from_owned(bytes)));
                    if let Some(list) = defs.families.get_mut(&FontFamily::Proportional) {
                        list.insert(0, "theme".into());
                    }
                }
                Err(e) => log::warn!("theme font {}: {e}", path.display()),
            }
        }
        ctx.set_fonts(defs);
    }
}

/// DM Sans, the UI typeface (SIL Open Font License, see
/// assets/fonts/DMSans-OFL.txt). Built into the exe so the UI always has
/// it: Medium for all text, Bold for headings and the wordmark.
static DM_SANS_MEDIUM: &[u8] = include_bytes!("../../assets/fonts/DMSans-Medium.ttf");
static DM_SANS_BOLD: &[u8] = include_bytes!("../../assets/fonts/DMSans-Bold.ttf");

/// Font family for headings and the "snag" wordmark.
pub fn brand_bold_family() -> FontFamily {
    FontFamily::Name("brand-bold".into())
}

/// egui's default fonts with DM Sans in front. egui's own fonts stay behind
/// it as fallbacks for symbols DM Sans doesn't have.
fn brand_fonts() -> FontDefinitions {
    let mut defs = FontDefinitions::default();
    defs.font_data.insert("dm-sans".into(), Arc::new(FontData::from_static(DM_SANS_MEDIUM)));
    defs.font_data.insert("dm-sans-bold".into(), Arc::new(FontData::from_static(DM_SANS_BOLD)));
    let fallbacks = defs.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    if let Some(list) = defs.families.get_mut(&FontFamily::Proportional) {
        list.insert(0, "dm-sans".into());
    }
    let mut bold = vec!["dm-sans-bold".to_string()];
    bold.extend(fallbacks);
    defs.families.insert(brand_bold_family(), bold);
    defs
}

/// Modification time, for hot-reloading a theme file while you edit it.
pub fn mtime(id: &str) -> Option<std::time::SystemTime> {
    std::fs::metadata(path_of(id)).and_then(|m| m.modified()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_themes_parse() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("themes");
        let mut n = 0;
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            if e.path().extension().is_some_and(|x| x == "json") {
                load_file(&e.path()).unwrap_or_else(|err| panic!("{err:#}"));
                n += 1;
            }
        }
        assert!(n >= 3, "ship at least 3 themes");
    }

    #[test]
    fn bom_is_tolerated() {
        let path = std::env::temp_dir().join(format!("snag-bom-{}.json", std::process::id()));
        std::fs::write(&path, "\u{feff}{\"name\":\"BOM\"}").unwrap();
        assert_eq!(load_file(&path).unwrap().name, "BOM");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn partial_theme_uses_defaults() {
        let t: Theme = serde_json::from_str(r##"{"colors":{"accent":"#ff008080"}}"##).unwrap();
        assert_eq!(t.colors.accent.0, Color32::from_rgba_unmultiplied(255, 0, 128, 128));
        assert_eq!(t.colors.text, Colors::default().text);
        assert!(parse_hex("#12345").is_none());
    }
}
