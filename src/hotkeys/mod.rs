//! Hotkeys: parsing/formatting key combinations and registering them.
//!
//! Any key works, with or without Ctrl/Alt/Shift/Win, including keys that
//! normally do something else (Print Screen, F12, a plain letter) and even a
//! lone modifier key like "RightCtrl". Two mechanisms make that possible:
//!
//! * `RegisterHotKey` (Windows' own hotkey system) whenever Windows allows
//!   it: zero cost per keystroke, and it works while any app is focused.
//! * A low-level keyboard hook (see `app::keyhook`) for everything Windows
//!   refuses: keys another program already registered, keys Windows keeps
//!   for itself, and lone modifiers. The hook sees each key first and
//!   swallows it, so Snag wins over the other program.
//!
//! Hotkeys are stored in settings as text like `"Ctrl+Shift+F10"`.

use std::fmt;

use anyhow::{Result, anyhow, bail};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
    UnregisterHotKey,
};

pub const ID_CLIP: i32 = 1;
pub const ID_SCREENSHOT: i32 = 2;

/// Escape is reserved for cancelling hotkey capture.
pub const VK_ESCAPE: u32 = 0x1B;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Hotkey {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    /// Windows virtual-key code.
    pub vk: u32,
}

/// Which modifier keys are held.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
}

/// Key names <-> virtual-key codes. Letters, digits, F-keys and numpad digits
/// are handled in code; anything else round-trips as `VK0xNN`.
const NAMED_KEYS: &[(&str, u32)] = &[
    ("Space", 0x20), ("Tab", 0x09), ("Enter", 0x0D), ("Backspace", 0x08), ("Escape", 0x1B),
    ("Insert", 0x2D), ("Delete", 0x2E), ("Home", 0x24), ("End", 0x23),
    ("PageUp", 0x21), ("PageDown", 0x22), ("Left", 0x25), ("Up", 0x26),
    ("Right", 0x27), ("Down", 0x28), ("PrintScreen", 0x2C), ("Pause", 0x13),
    ("ScrollLock", 0x91), ("NumLock", 0x90), ("CapsLock", 0x14), ("Menu", 0x5D), ("Clear", 0x0C),
    ("NumMultiply", 0x6A), ("NumAdd", 0x6B), ("NumSeparator", 0x6C),
    ("NumSubtract", 0x6D), ("NumDecimal", 0x6E), ("NumDivide", 0x6F),
    ("Minus", 0xBD), ("Equals", 0xBB), ("Comma", 0xBC), ("Period", 0xBE),
    ("Semicolon", 0xBA), ("Slash", 0xBF), ("Backtick", 0xC0),
    ("LeftBracket", 0xDB), ("Backslash", 0xDC), ("RightBracket", 0xDD), ("Quote", 0xDE),
    ("IntlBackslash", 0xE2),
    ("VolumeMute", 0xAD), ("VolumeDown", 0xAE), ("VolumeUp", 0xAF),
    ("MediaNext", 0xB0), ("MediaPrevious", 0xB1), ("MediaStop", 0xB2), ("MediaPlayPause", 0xB3),
    ("BrowserBack", 0xA6), ("BrowserForward", 0xA7), ("BrowserRefresh", 0xA8), ("BrowserStop", 0xA9),
    ("BrowserSearch", 0xAA), ("BrowserFavorites", 0xAB), ("BrowserHome", 0xAC),
    ("LaunchMail", 0xB4), ("LaunchMedia", 0xB5), ("LaunchApp1", 0xB6), ("LaunchApp2", 0xB7),
    ("Sleep", 0x5F),
    // Modifier keys on their own (left/right are distinct keys).
    ("LeftShift", 0xA0), ("RightShift", 0xA1), ("LeftCtrl", 0xA2), ("RightCtrl", 0xA3),
    ("LeftAlt", 0xA4), ("RightAlt", 0xA5), ("LeftWin", 0x5B), ("RightWin", 0x5C),
];

/// True for Shift/Ctrl/Alt/Win in any of their generic or left/right forms.
pub fn is_modifier_vk(vk: u32) -> bool {
    matches!(vk, 0x10 | 0x11 | 0x12 | 0xA0..=0xA5 | 0x5B | 0x5C)
}

fn key_from_name(name: &str) -> Option<u32> {
    let upper = name.to_ascii_uppercase();
    if upper.len() == 1 {
        let c = upper.as_bytes()[0];
        if c.is_ascii_uppercase() || c.is_ascii_digit() {
            return Some(c as u32); // VK_A..VK_Z / VK_0..VK_9 equal ASCII
        }
    }
    if let Some(n) = upper.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()) {
        if (1..=24).contains(&n) {
            return Some(0x70 + n - 1); // VK_F1 = 0x70
        }
    }
    if let Some(n) = upper.strip_prefix("NUM").and_then(|n| n.parse::<u32>().ok()) {
        if n <= 9 {
            return Some(0x60 + n); // VK_NUMPAD0 = 0x60
        }
    }
    if let Some(hex) = upper.strip_prefix("VK0X") {
        return u32::from_str_radix(hex, 16).ok().filter(|v| (1..=0xFE).contains(v));
    }
    NAMED_KEYS
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, vk)| *vk)
}

pub fn key_name(vk: u32) -> String {
    match vk {
        0x41..=0x5A | 0x30..=0x39 => char::from_u32(vk).unwrap().to_string(),
        0x70..=0x87 => format!("F{}", vk - 0x70 + 1),
        0x60..=0x69 => format!("Num{}", vk - 0x60),
        // Generic modifier codes are only seen if typed into settings.json.
        0x10 => "LeftShift".into(),
        0x11 => "LeftCtrl".into(),
        0x12 => "LeftAlt".into(),
        _ => NAMED_KEYS
            .iter()
            .find(|(_, v)| *v == vk)
            .map(|(n, _)| n.to_string())
            .unwrap_or_else(|| format!("VK0x{vk:02X}")),
    }
}

impl Hotkey {
    /// Parses `"Ctrl+Alt+F10"`, `"PrintScreen"`, `"RightCtrl"`, `"VK0xE8"`...
    /// Names are case-insensitive.
    pub fn parse(text: &str) -> Result<Hotkey> {
        let mut hk = Hotkey::default();
        let mut key = None;
        let parts: Vec<&str> = text.split('+').map(str::trim).filter(|p| !p.is_empty()).collect();
        for (i, part) in parts.iter().enumerate() {
            let last = i + 1 == parts.len();
            match part.to_ascii_lowercase().as_str() {
                // A modifier name is a modifier unless it's the only/last part,
                // in which case it's the key itself ("Shift" alone = LeftShift).
                "ctrl" | "control" if !last => hk.ctrl = true,
                "alt" if !last => hk.alt = true,
                "shift" if !last => hk.shift = true,
                "win" | "super" | "meta" if !last => hk.win = true,
                "ctrl" | "control" => key = Some(0xA2),
                "alt" => key = Some(0xA4),
                "shift" => key = Some(0xA0),
                "win" | "super" | "meta" => key = Some(0x5B),
                _ => {
                    if key.is_some() {
                        bail!("\"{text}\" has more than one non-modifier key");
                    }
                    key = Some(key_from_name(part).ok_or_else(|| anyhow!("unknown key \"{part}\""))?);
                }
            }
        }
        hk.vk = key.ok_or_else(|| anyhow!("\"{text}\" has no key"))?;
        if hk.vk == VK_ESCAPE {
            bail!("Esc is reserved for cancelling");
        }
        Ok(hk)
    }

    /// A lone modifier key ("RightCtrl"): Windows can't register these, so
    /// they always go through the keyboard hook, and the key isn't blocked.
    pub fn is_modifier_only(&self) -> bool {
        is_modifier_vk(self.vk)
    }

    fn modifiers(&self) -> HOT_KEY_MODIFIERS {
        let mut m = MOD_NOREPEAT; // holding the key down fires once, not repeatedly
        if self.ctrl {
            m |= MOD_CONTROL;
        }
        if self.alt {
            m |= MOD_ALT;
        }
        if self.shift {
            m |= MOD_SHIFT;
        }
        if self.win {
            m |= MOD_WIN;
        }
        m
    }

    pub fn mods(&self) -> Mods {
        Mods { ctrl: self.ctrl, alt: self.alt, shift: self.shift, win: self.win }
    }

    /// Does a key-down of `vk` with `held` modifiers trigger this hotkey?
    pub fn matches(&self, vk: u32, held: Mods) -> bool {
        if self.is_modifier_only() {
            // The modifier's own key-down isn't in `held` yet; any *other*
            // modifier held means it's part of a different shortcut.
            vk == self.vk && held == Mods::default()
        } else {
            vk == self.vk && held == self.mods()
        }
    }

    /// Builds a hotkey from a key name as produced by the UI's key capture.
    pub fn from_parts(ctrl: bool, alt: bool, shift: bool, win: bool, key: &str) -> Option<Hotkey> {
        let vk = key_from_name(key)?;
        Some(Hotkey { ctrl, alt, shift, win, vk })
    }

    /// A key that types or does something on its own (no modifier). Bound
    /// as a hotkey, it stops reaching other apps while Snag runs.
    pub fn is_plain_key(&self) -> bool {
        !(self.ctrl || self.alt || self.shift || self.win) && !self.is_modifier_only()
    }
}

impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("Ctrl".to_string());
        }
        if self.alt {
            parts.push("Alt".to_string());
        }
        if self.shift {
            parts.push("Shift".to_string());
        }
        if self.win {
            parts.push("Win".to_string());
        }
        parts.push(key_name(self.vk));
        write!(f, "{}", parts.join("+"))
    }
}

/// Tries Windows' own hotkey registration for `hk` under `id`.
/// Fails if another program owns the combination, Windows reserves it, or
/// it's a lone modifier; the caller then uses the keyboard hook instead.
/// Must be called on the thread that owns `hwnd`.
pub fn register(hwnd: HWND, id: i32, hk: &Hotkey) -> Result<()> {
    unregister(hwnd, id);
    if hk.is_modifier_only() {
        bail!("{hk} is a modifier key on its own");
    }
    // SAFETY: plain Win32 call.
    unsafe { RegisterHotKey(Some(hwnd), id, hk.modifiers(), hk.vk) }
        .map_err(|_| anyhow!("{hk} is taken by another program or reserved by Windows"))
}

pub fn unregister(hwnd: HWND, id: i32) {
    // SAFETY: unregistering an unknown id is harmless.
    unsafe {
        let _ = UnregisterHotKey(Some(hwnd), id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for s in [
            "Ctrl+F10", "Ctrl+Alt+Shift+S", "Alt+Num5", "Win+PrintScreen", "Ctrl+Backtick", "F12", "PrintScreen",
            "A", "RightCtrl", "LeftAlt", "MediaPlayPause", "Shift+VK0xE8", "F24",
        ] {
            assert_eq!(Hotkey::parse(s).unwrap().to_string(), s);
        }
        assert_eq!(Hotkey::parse("ctrl + f10").unwrap().to_string(), "Ctrl+F10");
        assert_eq!(Hotkey::parse("Shift").unwrap().to_string(), "LeftShift");
        assert!(Hotkey::parse("Escape").is_err(), "Esc stays reserved for cancel");
        assert!(Hotkey::parse("Ctrl+A+B").is_err());
        assert!(Hotkey::parse("Ctrl+").is_err() || Hotkey::parse("Ctrl+").unwrap().vk == 0xA2);
    }

    #[test]
    fn matching() {
        let none = Mods::default();
        let ctrl = Mods { ctrl: true, ..none };
        let f10 = Hotkey::parse("Ctrl+F10").unwrap();
        assert!(f10.matches(0x79, ctrl));
        assert!(!f10.matches(0x79, none));
        assert!(!f10.matches(0x79, Mods { shift: true, ..ctrl }));
        let rctrl = Hotkey::parse("RightCtrl").unwrap();
        assert!(rctrl.is_modifier_only() && rctrl.matches(0xA3, none));
        assert!(!rctrl.matches(0xA2, none));
        assert!(Hotkey::parse("P").unwrap().is_plain_key());
        assert!(!Hotkey::parse("RightAlt").unwrap().is_plain_key());
    }
}
