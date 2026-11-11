//! Low-level keyboard hook (`WH_KEYBOARD_LL`) on the main thread.
//!
//! Used for two things, and only installed while one of them is needed, so
//! normally it costs nothing:
//! * hotkeys Windows refuses to register (taken by another program,
//!   reserved by Windows like Print Screen, or a lone modifier key). The
//!   hook sees every key before anyone else and swallows a match, so the
//!   hotkey wins over whatever else wanted that key;
//! * "press the new hotkey" in Settings: the next key press is recorded and
//!   swallowed, whatever key it is. Esc cancels.
//!
//! Limitation (Windows security): a hook doesn't receive keys while a
//! program running as administrator is focused, unless Snag also runs as
//! administrator. Hotkeys registered the normal way don't have that limit.
//!
//! The hook procedure runs synchronously for every key press system-wide, so
//! it only does a few comparisons and posts events; Windows removes hooks
//! that take too long.

use std::cell::RefCell;
use std::collections::HashSet;

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    WM_KEYDOWN, WM_SYSKEYDOWN,
};

use super::controller::Event;
use crate::hotkeys::{Hotkey, Mods, VK_ESCAPE, is_modifier_vk};

/// An unassigned virtual-key code. Tapping it while Alt or Win is held stops
/// Windows from treating the Alt/Win release as "open the menu bar / Start".
const DUMMY_VK: u32 = 0xE8;

struct Capture {
    target: String,
    /// Modifier keys (left/right specific) held during capture.
    mods_down: HashSet<u32>,
    /// A modifier pressed alone: if it's released with nothing else pressed,
    /// the modifier itself becomes the hotkey.
    lone: Option<u32>,
}

#[derive(Default)]
struct HookState {
    hook: Option<HHOOK>,
    bindings: Vec<(i32, Hotkey)>,
    /// Bindings whose key is currently held (fire once per press, not on
    /// auto-repeat).
    held: HashSet<i32>,
    /// Keys whose key-down we swallowed: swallow their key-up too.
    swallow_up: HashSet<u32>,
    capture: Option<Capture>,
    /// Diagnostics/tests: how many hotkey presses the hook fired, and the
    /// last capture result.
    fired: u64,
    last_capture: Option<(String, Option<Hotkey>)>,
}

#[derive(Debug, PartialEq)]
enum Verdict {
    Pass,
    Swallow,
    /// Swallow, and tap the dummy key so a held Alt/Win doesn't open a menu.
    SwallowMasked,
}

thread_local! {
    static STATE: RefCell<HookState> = RefCell::new(HookState::default());
}

/// Generic modifier codes (from injected input) -> the left-hand key.
fn normalize(vk: u32) -> u32 {
    match vk {
        0x10 => 0xA0,
        0x11 => 0xA2,
        0x12 => 0xA4,
        v => v,
    }
}

fn held_mods() -> Mods {
    // SAFETY: plain key-state queries. The high bit means "down".
    let down = |vk: i32| unsafe { GetAsyncKeyState(vk) } < 0;
    Mods {
        ctrl: down(0x11),
        alt: down(0x12),
        shift: down(0x10),
        win: down(0x5B) || down(0x5C),
    }
}

impl HookState {
    fn on_key(&mut self, vk: u32, down: bool) -> Verdict {
        let vk = normalize(vk);
        if self.capture.is_some() {
            return self.on_capture_key(vk, down);
        }
        if !down {
            self.held.retain(|id| !self.bindings.iter().any(|(b, hk)| b == id && hk.vk == vk));
            return if self.swallow_up.remove(&vk) { Verdict::Swallow } else { Verdict::Pass };
        }
        let mods = held_mods();
        for (id, hk) in &self.bindings {
            if !hk.matches(vk, mods) {
                continue;
            }
            if self.held.insert(*id) {
                self.fired += 1;
                super::send(Event::Hotkey(*id));
            }
            if hk.is_modifier_only() {
                // Never block a modifier: Shift/Ctrl/Alt keep working normally.
                return Verdict::Pass;
            }
            self.swallow_up.insert(vk);
            return if mods.alt || mods.win { Verdict::SwallowMasked } else { Verdict::Swallow };
        }
        Verdict::Pass
    }

    /// While capturing, every key is swallowed (nothing leaks to other apps,
    /// and Win doesn't open Start).
    fn on_capture_key(&mut self, vk: u32, down: bool) -> Verdict {
        let Some(c) = self.capture.as_mut() else { return Verdict::Pass };
        if is_modifier_vk(vk) {
            if down {
                if c.mods_down.insert(vk) {
                    c.lone = if c.mods_down.len() == 1 { Some(vk) } else { None };
                }
            } else {
                c.mods_down.remove(&vk);
                if c.lone == Some(vk) {
                    let hk = Hotkey { vk, ..Default::default() };
                    self.finish_capture(Some(hk));
                }
            }
            return Verdict::Swallow;
        }
        if down {
            let has = |a: u32, b: u32| c.mods_down.contains(&a) || c.mods_down.contains(&b);
            let result = (vk != VK_ESCAPE).then(|| Hotkey {
                ctrl: has(0xA2, 0xA3),
                alt: has(0xA4, 0xA5),
                shift: has(0xA0, 0xA1),
                win: has(0x5B, 0x5C),
                vk,
            });
            self.swallow_up.insert(vk);
            self.finish_capture(result);
        }
        Verdict::Swallow
    }

    fn finish_capture(&mut self, hotkey: Option<Hotkey>) {
        if let Some(c) = self.capture.take() {
            self.last_capture = Some((c.target.clone(), hotkey));
            log::info!("hotkey capture for {}: {}", c.target, hotkey.map(|h| h.to_string()).unwrap_or("cancelled".into()));
            super::send(Event::HotkeyCaptured { target: c.target, hotkey: hotkey.map(|h| h.to_string()) });
        }
        // Can't unhook from inside the hook procedure; do it right after.
        super::post_to_main(super::MainRequest::UpdateKeyHook);
    }
}

unsafe extern "system" fn hook_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for WH_KEYBOARD_LL, lParam points to a KBDLLHOOKSTRUCT.
        let kb = unsafe { &*(lp.0 as *const KBDLLHOOKSTRUCT) };
        let msg = wp.0 as u32;
        let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        if kb.vkCode != DUMMY_VK {
            // `try_borrow_mut`: never panic inside a system callback.
            let verdict = STATE.with(|s| s.try_borrow_mut().map(|mut s| s.on_key(kb.vkCode, down)).unwrap_or(Verdict::Pass));
            match verdict {
                Verdict::Pass => {}
                Verdict::Swallow => return LRESULT(1),
                Verdict::SwallowMasked => {
                    tap_dummy_key();
                    return LRESULT(1);
                }
            }
        }
    }
    // SAFETY: pass everything else on to the next hook.
    unsafe { CallNextHookEx(None, code, wp, lp) }
}

fn tap_dummy_key() {
    let key = |flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(DUMMY_VK as u16), wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    };
    let inputs = [key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)];
    // SAFETY: two well-formed keyboard inputs.
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

/// Installs the hook if bindings or a capture need it; removes it otherwise.
pub fn update_install() {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let needed = !s.bindings.is_empty() || s.capture.is_some();
        match (needed, s.hook) {
            (true, None) => {
                // SAFETY: installing a low-level hook from a thread that runs
                // a message loop (required for LL hooks).
                let hook = unsafe {
                    let module = GetModuleHandleW(None).ok();
                    SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), module.map(Into::into), 0)
                };
                match hook {
                    Ok(h) => {
                        log::info!("keyboard hook installed");
                        s.hook = Some(h);
                    }
                    Err(e) => log::error!("could not install the keyboard hook: {e}"),
                }
            }
            (false, Some(h)) => {
                // SAFETY: a hook we installed.
                unsafe {
                    let _ = UnhookWindowsHookEx(h);
                }
                s.hook = None;
                s.held.clear();
                log::info!("keyboard hook removed (not needed)");
            }
            _ => {}
        }
    });
}

/// Hotkeys the hook should handle (those Windows wouldn't register).
pub fn set_bindings(bindings: Vec<(i32, Hotkey)>) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.bindings = bindings;
        s.held.clear();
    });
    update_install();
}

/// Records the next key press as the hotkey for `target`.
pub fn begin_capture(target: String) {
    STATE.with(|s| {
        s.borrow_mut().capture = Some(Capture { target, mods_down: HashSet::new(), lone: None });
    });
    update_install();
}

pub fn cancel_capture() {
    STATE.with(|s| {
        if let Ok(mut s) = s.try_borrow_mut() {
            if s.capture.is_some() {
                s.finish_capture(None);
            }
        }
    });
}

pub fn uninstall() {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.bindings.clear();
        s.capture = None;
    });
    update_install();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds key events to a capturing hook; returns the captured hotkey text
    /// (`Some(None)` = cancelled, `None` = still waiting).
    fn capture(events: &[(u32, bool)]) -> Option<Option<String>> {
        let mut s = HookState {
            capture: Some(Capture { target: "clip".into(), mods_down: HashSet::new(), lone: None }),
            ..Default::default()
        };
        for &(vk, down) in events {
            assert_eq!(s.on_key(vk, down), Verdict::Swallow, "capture swallows every key");
            if s.capture.is_none() {
                break;
            }
        }
        s.last_capture.map(|(_, hk)| hk.map(|h| h.to_string()))
    }

    #[test]
    fn captures_any_key() {
        let some = |t: &str| Some(Some(t.to_string()));
        assert_eq!(capture(&[(0x78, true)]), some("F9"));
        assert_eq!(capture(&[(0x7B, true)]), some("F12"));
        assert_eq!(capture(&[(0x2C, true)]), some("PrintScreen"));
        assert_eq!(capture(&[(0x41, true)]), some("A"));
        assert_eq!(capture(&[(0xA2, true), (0xA0, true), (0x50, true)]), some("Ctrl+Shift+P"));
        assert_eq!(capture(&[(0x5B, true), (0x47, true)]), some("Win+G"));
        assert_eq!(capture(&[(0xA5, true), (0x73, true)]), some("Alt+F4"));
        // A modifier pressed and released alone becomes the hotkey.
        assert_eq!(capture(&[(0xA3, true), (0xA3, false)]), some("RightCtrl"));
        // ...but not if another modifier joined in.
        assert_eq!(capture(&[(0xA3, true), (0xA0, true), (0xA3, false)]), None);
        // Esc cancels.
        assert_eq!(capture(&[(0x1B, true)]), Some(None));
        assert_eq!(capture(&[(0xA2, true), (0x1B, true)]), Some(None));
    }

    #[test]
    fn hooked_hotkey_fires_once_per_press_and_swallows() {
        let mut s = HookState { bindings: vec![(1, Hotkey::parse("F9").unwrap())], ..Default::default() };
        assert_eq!(s.on_key(0x78, true), Verdict::Swallow);
        assert_eq!(s.on_key(0x78, true), Verdict::Swallow, "auto-repeat is swallowed too");
        assert_eq!(s.fired, 1, "auto-repeat doesn't fire again");
        assert_eq!(s.on_key(0x78, false), Verdict::Swallow, "key-up of a swallowed key");
        assert_eq!(s.on_key(0x78, true), Verdict::Swallow);
        assert_eq!(s.fired, 2);
        assert_eq!(s.on_key(0x79, true), Verdict::Pass, "other keys pass through");
    }

    #[test]
    fn lone_modifier_hotkey_fires_but_passes_through() {
        let mut s = HookState { bindings: vec![(2, Hotkey::parse("RightCtrl").unwrap())], ..Default::default() };
        assert_eq!(s.on_key(0xA3, true), Verdict::Pass, "Right Ctrl keeps working normally");
        assert_eq!(s.fired, 1);
        assert_eq!(s.on_key(0xA2, true), Verdict::Pass, "Left Ctrl doesn't trigger it");
        assert_eq!(s.fired, 1);
    }
}
