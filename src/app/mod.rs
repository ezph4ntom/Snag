//! The core process: tray icon, global hotkeys, focus tracking, recording.
//!
//! Threads:
//! * **main**: owns a hidden message-only window (receives `WM_HOTKEY`), the
//!   tray icon, and the foreground-window hook. It just runs the Win32
//!   message loop and forwards events. Win32 requires hotkeys, tray icons and
//!   WinEvent hooks to live on a thread with a message loop.
//! * **controller**: all decisions and state (see `controller.rs`).
//! * recorder threads (pacer, encoder writer), clip saver, workers.
//!
//! The controller asks the main thread to do main-thread-only work (register
//! hotkeys, update the tray) by queueing a `MainRequest` and posting a
//! window message to wake it.

pub mod controller;
pub mod ipc;
mod keyhook;
pub mod tray;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::hotkeys::{self, Hotkey, ID_CLIP, ID_SCREENSHOT};
use controller::{Controller, Event};

const CLASS_NAME: PCWSTR = w!("ClipAppCoreWindow");
const WM_APP_REQUEST: u32 = WM_APP + 1;
const WM_APP_SHOW: u32 = WM_APP + 2;
const WM_APP_QUIT: u32 = WM_APP + 3;

/// Work only the main thread may do.
pub enum MainRequest {
    RegisterHotkeys { clip: String, screenshot: String },
    /// Record the next key press as the hotkey for `target`.
    BeginHotkeyCapture { target: String },
    CancelHotkeyCapture,
    /// Install/remove the keyboard hook to match what's needed now.
    UpdateKeyHook,
    UpdateTray { recording: bool, tooltip: String, clip_key: String, shot_key: String },
    Quit,
}

static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);
static MAIN_QUEUE: Mutex<VecDeque<MainRequest>> = Mutex::new(VecDeque::new());
static EVENTS: OnceLock<Sender<Event>> = OnceLock::new();

thread_local! {
    // muda/tray-icon objects aren't Send, so they live in the main thread.
    static TRAY: RefCell<Option<tray::Tray>> = const { RefCell::new(None) };
}

/// Queues a request for the main thread and wakes it.
pub fn post_to_main(req: MainRequest) {
    MAIN_QUEUE.lock().unwrap_or_else(|p| p.into_inner()).push_back(req);
    let hwnd = HWND(MAIN_HWND.load(Ordering::Relaxed) as *mut _);
    // SAFETY: posting to our own window; harmless if it's already gone.
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_APP_REQUEST, WPARAM(0), LPARAM(0));
    }
}

fn send(ev: Event) {
    if let Some(tx) = EVENTS.get() {
        let _ = tx.send(ev);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_HOTKEY => send(Event::Hotkey(wp.0 as i32)),
        WM_APP_SHOW => send(Event::ShowUi),
        WM_APP_QUIT => send(Event::Tray(controller::TrayAction::Quit)),
        WM_APP_REQUEST => loop {
            let next = MAIN_QUEUE.lock().unwrap_or_else(|p| p.into_inner()).pop_front();
            match next {
                Some(req) => handle_request(hwnd, req),
                None => break,
            }
        },
        // SAFETY: default processing for everything else.
        _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
    LRESULT(0)
}

fn handle_request(hwnd: HWND, req: MainRequest) {
    match req {
        MainRequest::RegisterHotkeys { clip, screenshot } => {
            // Windows' own hotkey registration where possible; the keyboard
            // hook for anything Windows refuses, so any key can be a hotkey.
            let mut notes = Vec::new();
            let mut hooked = Vec::new();
            for (id, text, what) in [(ID_CLIP, clip, "Clip"), (ID_SCREENSHOT, screenshot, "Screenshot")] {
                match Hotkey::parse(&text) {
                    Err(e) => {
                        hotkeys::unregister(hwnd, id);
                        notes.push(format!("{what} hotkey \"{text}\": {e}"));
                    }
                    Ok(hk) => match hotkeys::register(hwnd, id, &hk) {
                        Ok(()) => log::info!("{what} hotkey: {hk}"),
                        Err(e) => {
                            log::info!("{what} hotkey: {hk} via keyboard hook ({e})");
                            hooked.push((id, hk));
                            notes.push(format!(
                                "{what} hotkey {hk} is caught with a keyboard hook (Windows wouldn't register it). It works everywhere except while a program running as administrator is focused."
                            ));
                        }
                    },
                }
            }
            keyhook::set_bindings(hooked);
            send(Event::HotkeyNotes(notes));
        }
        MainRequest::BeginHotkeyCapture { target } => keyhook::begin_capture(target),
        MainRequest::CancelHotkeyCapture => keyhook::cancel_capture(),
        MainRequest::UpdateKeyHook => keyhook::update_install(),
        MainRequest::UpdateTray { recording, tooltip, clip_key, shot_key } => TRAY.with(|t| {
            if let Some(t) = t.borrow().as_ref() {
                t.update(recording, &tooltip, &clip_key, &shot_key);
            }
        }),
        // SAFETY: called on the thread that runs the message loop.
        MainRequest::Quit => unsafe { PostQuitMessage(0) },
    }
}

/// Called by Windows (on the main thread, via the message loop) whenever the
/// foreground window changes or a window finishes moving.
unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if id_object != OBJID_WINDOW.0 {
        return;
    }
    // SAFETY: plain queries on whatever window is in front right now.
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() {
            return;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(fg, Some(&mut pid));
        let mut buf = [0u16; 128];
        let n = GetClassNameW(fg, &mut buf).max(0) as usize;
        send(Event::Foreground {
            monitor: crate::capture::monitor_of_window(fg),
            pid,
            class: String::from_utf16_lossy(&buf[..n]),
        });
    }
}

fn create_message_window() -> Result<HWND> {
    // SAFETY: standard class registration + message-only window creation.
    unsafe {
        let hinstance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        RegisterClassW(&class);
        // HWND_MESSAGE parent = invisible "message-only" window.
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS_NAME,
            w!("ClipApp"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinstance.into()),
            None,
        )
        .context("CreateWindowExW failed")
    }
}

/// `clipapp.exe --quit`: asks a running instance to exit cleanly (stops the
/// buffer, closes the window). Returns false if none is running.
pub fn quit_running_instance() -> bool {
    // SAFETY: finding and posting to another process's message window.
    unsafe {
        match FindWindowExW(Some(HWND_MESSAGE), None, CLASS_NAME, PCWSTR::null()) {
            Ok(h) => PostMessageW(Some(h), WM_APP_QUIT, WPARAM(0), LPARAM(0)).is_ok(),
            Err(_) => false,
        }
    }
}

/// Entry point of the core (tray) process.
pub fn run_core() -> Result<()> {
    crate::win::set_dpi_awareness();
    crate::logging::init("core");
    log::info!("ClipApp {} starting (pid {})", env!("CARGO_PKG_VERSION"), std::process::id());

    // Single instance: a second launch just asks the first to show its window.
    // SAFETY: named mutex creation; the handle intentionally lives until exit.
    unsafe {
        let _mutex = CreateMutexW(None, true, w!("Local\\ClipApp.Core.SingleInstance"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(other) = FindWindowExW(Some(HWND_MESSAGE), None, CLASS_NAME, PCWSTR::null()) {
                let _ = PostMessageW(Some(other), WM_APP_SHOW, WPARAM(0), LPARAM(0));
            }
            log::info!("already running; asked the running instance to show its window");
            return Ok(());
        }
    }

    // Buffers left behind by a crash are useless now; start clean.
    let _ = std::fs::remove_dir_all(crate::paths::buffer_root());

    let (settings, first_launch) = crate::config::Settings::load();
    let (tx, rx) = mpsc::channel::<Event>();
    let _ = EVENTS.set(tx.clone());

    let hwnd = create_message_window()?;
    MAIN_HWND.store(hwnd.0 as isize, Ordering::Relaxed);

    let tray = tray::Tray::new(tx.clone()).context("creating tray icon")?;
    TRAY.with(|t| *t.borrow_mut() = Some(tray));

    // SAFETY: out-of-context hooks call us back through our message loop.
    let hooks: Vec<HWINEVENTHOOK> = unsafe {
        [EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MOVESIZEEND]
            .into_iter()
            .map(|e| SetWinEventHook(e, e, None, Some(win_event_proc), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS))
            .collect()
    };

    let controller = Controller::new(settings, first_launch, tx);
    let ctl = std::thread::Builder::new()
        .name("controller".into())
        .spawn(move || controller.run(rx))?;

    // The message loop. Blocks (zero CPU) until a message arrives.
    let mut msg = MSG::default();
    // SAFETY: standard Win32 message loop.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        for h in hooks {
            let _ = UnhookWinEvent(h);
        }
    }
    keyhook::uninstall();
    TRAY.with(|t| t.borrow_mut().take());
    let _ = ctl.join();
    log::info!("bye");
    Ok(())
}
