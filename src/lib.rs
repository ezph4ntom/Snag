//! ClipApp: a lightweight game-clipping app for Windows.
//!
//! The crate is a library so the integration tests in `tests/` can drive the
//! real pipeline; `src/main.rs` is only an argument parser that picks a mode.
//!
//! Process model (see README "Architecture"):
//! * **Core process** (`clipapp.exe`): tray icon, global hotkeys, capture,
//!   encoding, replay buffer. It has no UI window, so its idle cost stays low.
//! * **UI process** (`clipapp.exe --ui`): the egui window with the trimmer,
//!   settings, quality and theme tabs. It is started on demand by the core and
//!   exits when closed, so the GPU context and fonts are only in memory while
//!   you're actually looking at it.

#![cfg(windows)]
// Nested `if let` blocks are kept on purpose: for readers new to Rust they
// are easier to follow than the newer `if let ... && ...` chains.
#![allow(clippy::collapsible_if)]

pub mod app;
pub mod audio;
pub mod buffer;
pub mod capture;
pub mod config;
pub mod encode;
pub mod hotkeys;
pub mod logging;
pub mod paths;
pub mod sound;
pub mod themes;
pub mod trimmer;
pub mod tune;
pub mod ui;
pub mod verify;
pub mod win;
