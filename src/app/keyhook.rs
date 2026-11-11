//! Low-level keyboard hook (`WH_KEYBOARD_LL`) and raw mouse input, on the
//! main thread.
//!
//! Every hotkey goes through the hook, and Windows' own `RegisterHotKey` is
//! a second line where Windows allows it:
//! * the hook sees every key before anyone else and swallows a match, so the
//!   hotkey wins over whatever else wanted that key, and it keeps working in
//!   games that switch off ordinary app hotkeys while they're focused (games
//!   that read the keyboard with raw input often do: `RIDEV_NOHOTKEYS`);
//! * a hook gets no keys while a program running as administrator is
//!   focused (Windows security), but `RegisterHotKey` hotkeys still arrive
//!   then. When both see a press, it only counts once.
//!
//! Also here: "press the new hotkey" in Settings (the next key press is
//! recorded and swallowed, whatever key it is; Esc cancels) and mouse-button
//! hotkeys (middle, back, forward), which come from raw input since there's
//! no hotkey API for the mouse. Raw input is only switched on while a mouse
//! button is bound or a capture is running.
//!
//! The hook procedure runs synchronously for every key press system-wide, so
//! it only does a few comparisons and posts events. Windows silently removes
//! a hook that is too slow even once in a while, so it's re-installed every
//! 30 seconds as a precaution (see `refresh`).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    VIRTUAL_KEY,
};
use windows::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT, RIDEV_INPUTSINK, RIDEV_REMOVE,
    RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    WM_KEYDOWN, WM_SYSKEYDOWN,
};

use super::controller::Event;
use crate::hotkeys::{Hotkey, Mods, VK_ESCAPE, is_modifier_vk, is_mouse_vk};

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
    installed_at: Option<Instant>,
    /// Raw mouse input is registered (for mouse-button hotkeys/capture).
    raw_mouse: bool,
    /// The Left Ctrl that Windows fakes before Right Alt on AltGr keyboard
    /// layouts (Portuguese, German, French...) is down. It isn't a real Ctrl.
    altgr_ctrl: bool,
    /// When the hook last fired each hotkey, so the same press arriving
    /// again as a `WM_HOTKEY` isn't counted twice.
    last_fire: HashMap<i32, Instant>,
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

fn held_mods(altgr_ctrl: bool) -> Mods {
    // SAFETY: plain key-state queries. The high bit means "down".
    let down = |vk: i32| unsafe { GetAsyncKeyState(vk) } < 0;
    Mods {
        ctrl: (down(0xA2) && !altgr_ctrl) || down(0xA3),
        alt: down(0x12),
        shift: down(0x10),
        win: down(0x5B) || down(0x5C),
    }
}

/// The fake Left Ctrl that comes with AltGr has scan code 0x21D.
fn is_altgr_fake_ctrl(vk: u32, scan: u32) -> bool {
    vk == 0xA2 && scan & 0x200 != 0
}

impl HookState {
    fn fire(&mut self, id: i32) {
        self.fired += 1;
        self.last_fire.insert(id, Instant::now());
        log::info!("hotkey {id} pressed (keyboard hook / raw input)");
        super::send(Event::Hotkey(id));
    }

    fn on_key(&mut self, vk: u32, scan: u32, down: bool) -> Verdict {
        if is_altgr_fake_ctrl(vk, scan) {
            self.altgr_ctrl = down;
            return if self.capture.is_some() { Verdict::Swallow } else { Verdict::Pass };
        }
        let vk = normalize(vk);
        if self.capture.is_some() {
            return self.on_capture_key(vk, down);
        }
        if !down {
            self.held.retain(|id| !self.bindings.iter().any(|(b, hk)| b == id && hk.vk == vk));
            return if self.swallow_up.remove(&vk) { Verdict::Swallow } else { Verdict::Pass };
        }
        let mods = held_mods(self.altgr_ctrl);
        let hit = self.bindings.iter().find(|(_, hk)| hk.matches(vk, mods)).copied();
        if let Some((id, hk)) = hit {
            if self.held.insert(id) {
                self.fire(id);
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

    /// A mouse button (pseudo virtual-key 0x04..0x06) from raw input. Raw
    /// input can't swallow a click; the game still gets it too.
    fn on_mouse_button(&mut self, vk: u32, down: bool, mods: Mods) {
        if let Some(c) = &self.capture {
            if down {
                let has = |a: u32, b: u32| c.mods_down.contains(&a) || c.mods_down.contains(&b);
                let hk = Hotkey {
                    ctrl: has(0xA2, 0xA3),
                    alt: has(0xA4, 0xA5),
                    shift: has(0xA0, 0xA1),
                    win: has(0x5B, 0x5C),
                    vk,
                };
                self.finish_capture(Some(hk));
            }
            return;
        }
        if !down {
            self.held.retain(|id| !self.bindings.iter().any(|(b, hk)| b == id && hk.vk == vk));
            return;
        }
        let hit = self.bindings.iter().find(|(_, hk)| hk.matches(vk, mods)).map(|(id, _)| *id);
        if let Some(id) = hit {
            if self.held.insert(id) {
                self.fire(id);
            }
        }
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
            let verdict = STATE.with(|s| {
                s.try_borrow_mut().map(|mut s| s.on_key(kb.vkCode, kb.scanCode, down)).unwrap_or(Verdict::Pass)
            });
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
/// Same for raw mouse input. Returns an error text if something is needed
/// but Windows wouldn't set it up.
pub fn update_install() -> Option<String> {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let needed = !s.bindings.is_empty() || s.capture.is_some();
        let mut error = None;
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
                        log::debug!("keyboard hook installed");
                        s.hook = Some(h);
                        s.installed_at = Some(Instant::now());
                    }
                    Err(e) => {
                        log::error!("could not install the keyboard hook: {e}");
                        error = Some(format!("Windows refused Snag's keyboard hook ({})", e.message()));
                    }
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
        let mouse = s.capture.is_some() || s.bindings.iter().any(|(_, hk)| is_mouse_vk(hk.vk));
        if mouse != s.raw_mouse {
            match register_raw_mouse(mouse) {
                Ok(()) => {
                    s.raw_mouse = mouse;
                    log::debug!("raw mouse input {}", if mouse { "on" } else { "off" });
                }
                Err(e) => {
                    log::error!("raw mouse input: {e}");
                    if mouse {
                        error = Some(format!("Windows refused mouse-button input ({})", e.message()));
                    }
                }
            }
        }
        error
    })
}

fn register_raw_mouse(on: bool) -> windows::core::Result<()> {
    let hwnd = HWND(super::MAIN_HWND.load(std::sync::atomic::Ordering::Relaxed) as *mut _);
    let dev = RAWINPUTDEVICE {
        usUsagePage: 0x01, // generic desktop
        usUsage: 0x02,     // mouse
        // INPUTSINK: also while another app (the game) has focus.
        dwFlags: if on { RIDEV_INPUTSINK } else { RIDEV_REMOVE },
        hwndTarget: if on { hwnd } else { HWND::default() },
    };
    // SAFETY: one well-formed device description.
    unsafe { RegisterRawInputDevices(&[dev], std::mem::size_of::<RAWINPUTDEVICE>() as u32) }
}

/// `WM_INPUT` for the main window: mouse buttons for hotkeys.
pub fn on_raw_input(lp: LPARAM) {
    let mut raw = RAWINPUT::default();
    let mut size = std::mem::size_of::<RAWINPUT>() as u32;
    // SAFETY: `raw` is large enough for a mouse packet and `size` says so.
    let n = unsafe {
        GetRawInputData(
            HRAWINPUT(lp.0 as *mut _),
            RID_INPUT,
            Some(&mut raw as *mut _ as *mut _),
            &mut size,
            std::mem::size_of::<RAWINPUTHEADER>() as u32,
        )
    };
    if n == u32::MAX || raw.header.dwType != RIM_TYPEMOUSE.0 {
        return;
    }
    // SAFETY: dwType says this is the mouse variant.
    let flags = unsafe { raw.data.mouse.Anonymous.Anonymous.usButtonFlags } as u32;
    if flags == 0 {
        return; // movement only (the vast majority of packets)
    }
    // RI_MOUSE_* button flags: (down, up, pseudo virtual-key).
    const BUTTONS: [(u32, u32, u32); 3] = [(0x0010, 0x0020, 0x04), (0x0040, 0x0080, 0x05), (0x0100, 0x0200, 0x06)];
    STATE.with(|s| {
        let Ok(mut s) = s.try_borrow_mut() else { return };
        let mods = held_mods(s.altgr_ctrl);
        for (down, up, vk) in BUTTONS {
            if flags & down != 0 {
                s.on_mouse_button(vk, true, mods);
            }
            if flags & up != 0 {
                s.on_mouse_button(vk, false, mods);
            }
        }
    });
}

/// True if the hook fired hotkey `id` within the last half second (so a
/// `WM_HOTKEY` for the same press is a duplicate).
pub fn fired_recently(id: i32) -> bool {
    STATE.with(|s| {
        s.try_borrow()
            .ok()
            .and_then(|s| s.last_fire.get(&id).map(|t| t.elapsed() < Duration::from_millis(500)))
            .unwrap_or(false)
    })
}

/// Re-installs the hook (called every 30 s): Windows silently drops a
/// low-level hook that answers too slowly even once (e.g. while the PC is
/// swapping hard), and there's no notification when it does.
pub fn refresh() {
    let due = STATE.with(|s| {
        let s = s.borrow();
        s.hook.is_some() && s.capture.is_none() && s.installed_at.is_some_and(|t| t.elapsed() >= Duration::from_secs(30))
    });
    if !due {
        return;
    }
    STATE.with(|s| {
        if let Some(h) = s.borrow_mut().hook.take() {
            // SAFETY: a hook we installed.
            unsafe {
                let _ = UnhookWindowsHookEx(h);
            }
        }
    });
    if let Some(e) = update_install() {
        log::error!("keyboard hook refresh failed: {e}");
    }
}

/// Hotkeys the hook should handle (those Windows wouldn't register).
pub fn set_bindings(bindings: Vec<(i32, Hotkey)>) -> Option<String> {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.bindings = bindings;
        s.held.clear();
    });
    update_install()
}

/// Records the next key press as the hotkey for `target`.
pub fn begin_capture(target: String) {
    STATE.with(|s| {
        s.borrow_mut().capture = Some(Capture { target, mods_down: HashSet::new(), lone: None });
    });
    if let Some(e) = update_install() {
        log::error!("hotkey capture: {e}");
    }
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
    let _ = update_install();
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
            assert_eq!(s.on_key(vk, 0x1D, down), Verdict::Swallow, "capture swallows every key");
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
        assert_eq!(s.on_key(0x78, 0x44, true), Verdict::Swallow);
        assert_eq!(s.on_key(0x78, 0x44, true), Verdict::Swallow, "auto-repeat is swallowed too");
        assert_eq!(s.fired, 1, "auto-repeat doesn't fire again");
        assert_eq!(s.on_key(0x78, 0x44, false), Verdict::Swallow, "key-up of a swallowed key");
        assert_eq!(s.on_key(0x78, 0x44, true), Verdict::Swallow);
        assert_eq!(s.fired, 2);
        assert_eq!(s.on_key(0x79, 0x57, true), Verdict::Pass, "other keys pass through");
    }

    #[test]
    fn lone_modifier_hotkey_fires_but_passes_through() {
        let mut s = HookState { bindings: vec![(2, Hotkey::parse("RightCtrl").unwrap())], ..Default::default() };
        assert_eq!(s.on_key(0xA3, 0x1D, true), Verdict::Pass, "Right Ctrl keeps working normally");
        assert_eq!(s.fired, 1);
        assert_eq!(s.on_key(0xA2, 0x1D, true), Verdict::Pass, "Left Ctrl doesn't trigger it");
        assert_eq!(s.fired, 1);
    }

    #[test]
    fn altgr_counts_as_right_alt_alone() {
        // AltGr on a Portuguese/German layout: a fake Left Ctrl (scan code
        // 0x21D), then Right Alt. Captured, that's "RightAlt", not Ctrl+Alt.
        let mut s = HookState {
            capture: Some(Capture { target: "clip".into(), mods_down: HashSet::new(), lone: None }),
            ..Default::default()
        };
        for (vk, scan, down) in [(0xA2, 0x21D, true), (0xA5, 0x38, true), (0xA2, 0x21D, false), (0xA5, 0x38, false)] {
            s.on_key(vk, scan, down);
        }
        assert_eq!(s.last_capture.map(|(_, h)| h.map(|h| h.to_string())), Some(Some("RightAlt".into())));
    }

    #[test]
    fn mouse_buttons() {
        let none = Mods::default();
        let mut s = HookState { bindings: vec![(1, Hotkey::parse("Mouse4").unwrap())], ..Default::default() };
        s.on_mouse_button(0x05, true, none);
        s.on_mouse_button(0x05, true, none);
        assert_eq!(s.fired, 1, "a held button fires once");
        s.on_mouse_button(0x05, false, none);
        s.on_mouse_button(0x06, true, none);
        assert_eq!(s.fired, 1, "other buttons don't fire");
        let mut c = HookState {
            capture: Some(Capture { target: "clip".into(), mods_down: [0xA2].into(), lone: None }),
            ..Default::default()
        };
        c.on_mouse_button(0x04, true, none);
        assert_eq!(c.last_capture.map(|(_, h)| h.map(|h| h.to_string())), Some(Some("Ctrl+MouseMiddle".into())));
    }
}
