# Snag

A lightweight game-clipping app for Windows 10/11, written in Rust.
(Called ClipApp before v0.3; settings carry over automatically.)

* **Replay buffer**: always keeps the last N seconds (default 30) of video,
  **PC audio and your microphone**. Press **Ctrl+F10** and they're saved as
  an MP4 without interrupting recording.
* **Sound mixer**: record the whole PC (every output device, e.g. a
  headset's separate "Game" and "Chat" devices) or only the apps you tick
  (Spotify, the game, Discord...), each with its own volume and live meter.
* **Recording** for long sessions, two ways: **Start recording / Stop
  recording** (button or **Ctrl+F9**), or **record the whole session** from
  the moment Snag starts and save it whenever you like. It reuses the replay
  buffer's encoder, so it adds no extra load.
* **Live preview** of what's being recorded (sharp, ~30 fps; switch it off
  and it costs nothing).
* **Screenshots**: **Ctrl+F11** saves a PNG of the monitor being captured.
* **Any key as a hotkey**: F-keys, letters, Print Screen, F12, a lone Right
  Ctrl, the middle/side mouse buttons, with or without modifiers, even keys
  another program already uses. Problems are shown in Settings and logged.
* **Follows your focus**: captures whichever monitor holds the active
  window, and switches when you Alt-Tab to another monitor (or capture a fixed monitor instead).
* **Hardware encoding**: NVIDIA NVENC, AMD AMF, Intel Quick Sync, with x264 as fallback.
* **Built-in trimmer**: timeline with draggable handles, preview, frame
  stepping, lossless "fast" cut or frame-exact "precise" cut.
* **Auto-tuned quality**: detects your GPU/encoders/CPU/RAM/monitors and
  picks Quality/Balanced/Performance, and explains why.
* **Themes**: JSON files, hot-reloaded. Daylight (default, light), Graphite,
  Midnight, Warm Ink and OLED included. Snag draws its own title bar in the
  theme (Windows snap, edge resizing and double-click maximize still work).
* **Your layout**: drag the tab bar to any window edge, show tabs as icons
  only, and set the UI size with − / + in Settings → Window.

Measured on the development PC (RTX 3070 Ti, 20 threads, 1080p @ 144 Hz),
recording 1080p60 NVENC + PC audio: **~1.8 % total CPU and ~95 MB private
memory** (core 0.5 % / 39 MB + ffmpeg 1.3 % / 57 MB). Without audio: ~1.1 %
and ~90 MB. The UI window is a separate process that only exists while it's open.

**Testing it on another PC?** See [TESTING.md](TESTING.md): it's written for testers.

---

## 1. Build and run

### Requirements

| What | Why | Install |
|---|---|---|
| Windows 10 1903+ or Windows 11 | Windows Graphics Capture API | (Windows 11 24H2+ recommended, see limitations) |
| Rust (stable, MSVC toolchain) | compiler | `winget install Rustlang.Rustup` (then open a **new** terminal) |
| Visual Studio 2022 Build Tools, "Desktop development with C++" + a Windows SDK | Rust's MSVC linker | `winget install Microsoft.VisualStudio.2022.BuildTools --override "--add Microsoft.VisualStudio.Workload.VCTools --includeRecommended --passive"` |
| ffmpeg **full** build (with `h264_nvenc`, `h264_amf`, `h264_qsv`, `libx264`) | encoding sidecar | `winget install Gyan.FFmpeg` |
| Up-to-date GPU driver | hardware encoder | from NVIDIA / AMD / Intel |

Any of these already installed? Skip it. Check with `rustc --version`,
`cargo --version`, `ffmpeg -version`.

### Steps

```powershell
cd C:\clipApp

# 1. (Optional) bundle ffmpeg next to the app; otherwise it's found on PATH
powershell -ExecutionPolicy Bypass -File scripts\bundle-ffmpeg.ps1

# 2. Build (the first build compiles all dependencies: a few minutes)
cargo build --release

# 3. Run: a tray icon appears; on first launch the Quality tab opens
#    showing what auto-tune picked and why.
.\target\release\snag.exe
```

During development `cargo run` works too (debug builds keep a console
window with the live log). Other command-line options:

```powershell
snag.exe --detect   # print hardware detection + the three presets, then exit
snag.exe --quit     # ask the running instance to exit cleanly
```

The exe links the C runtime statically (`.cargo/config.toml`), so it runs
on PCs without the Visual C++ Redistributable.

### Installer (easiest for friends)

```powershell
winget install JRSoftware.InnoSetup                                 # once
powershell -ExecutionPolicy Bypass -File scripts\installer.ps1     # -> dist\Snag-Setup-<version>.exe
```

The installer needs no administrator rights. It installs to
`%LOCALAPPDATA%\Programs\Snag` and adds a Start menu shortcut, plus an
optional desktop shortcut and "start with Windows". If ffmpeg is missing,
it offers to install it with winget. It updates in place (a running Snag is
closed first) and has a normal uninstaller; settings and clips are kept.
The script is `installer\snag.iss`.

### Portable package (for testers)

```powershell
powershell -ExecutionPolicy Bypass -File scripts\package.ps1          # without ffmpeg (~5 MB zip)
powershell -ExecutionPolicy Bypass -File scripts\package.ps1 -WithFfmpeg
```

creates `dist\Snag\` and `dist\Snag-v<version>.zip`. Without ffmpeg,
testers install it once with `winget install Gyan.FFmpeg`; if it's missing
Snag shows a message with that exact command. (ffmpeg full builds are
GPL-licensed: bundling them means also shipping their license and a source link.)

### Running on another PC: hardware detection

Yes, detection works on any PC you move the app to, automatically:

* Settings live in that PC's `%APPDATA%\Snag`, not in the app folder,
  so a copied app starts fresh there: it detects the GPU (DXGI), runs a
  real test encode with every encoder (so it only picks one that actually
  works there), reads CPU threads, RAM and monitors, then picks a preset.
* It also re-detects by itself when the saved results don't fit the PC
  anymore: different GPU, or no working encoder last time (e.g. ffmpeg
  wasn't installed yet).
* The **Re-detect hardware** button in the Quality tab runs it on demand.

### Publishing a new version on GitHub

The repository is private; invite testers under *Settings → Collaborators*.

```powershell
# commit your changes
git add -A
git commit -m "Describe the change"
git push

# bump `version` in Cargo.toml, then build the zip and publish a release
powershell -ExecutionPolicy Bypass -File scripts\package.ps1
gh release create v0.3.0 dist\Snag-v0.3.0.zip --title "Snag v0.3.0" --notes "What changed..."
```

Testers report back through **Issues** (bug report and feedback templates
are in `.github/ISSUE_TEMPLATE`).

### Tests

```powershell
cargo test                                                         # unit tests (fast)
cargo test --release --test reliability -- --ignored --nocapture   # 50-clip tests (~2 min)
cargo test --release --test trimmer -- --ignored --nocapture       # trim exactness (~10 s)
```

* `fifty_clips_synthetic`: records a synthetic 1080p60 source whose every
  frame carries a barcode of its frame number, plus a beep at every whole
  second, and saves **50 clips in a row** (one per second, overlapping,
  while recording continues and while earlier clips are decoded on all
  cores). Every frame of every clip is checked: no black frames, no
  duplicates, no gaps, keyframe first, constant frame rate. The audio must
  cover the clip, and each beep must line up with the video frame from the
  same instant (A/V sync).
* `fifty_clips_live_capture`: the same through **real screen capture and
  real PC audio**: a 640x360 test-pattern window flashes in the top-left
  corner for about a minute while the primary monitor is recorded.
* `SNAG_TEST_ENCODER=nvenc|amf|qsv|x264` picks the encoder (default: best available).

Last results on the development PC (NVENC): **synthetic 50/50**, **live
50/50**, 0 dropped frames, 0 repeated frames, worst A/V offset 6 ms,
average save time 0.65 s. Trims: frame-exact in both modes, audio kept in sync.

---

## 2. Using it

| Action | How |
|---|---|
| Save the last N seconds | **Ctrl+F10** (tray: *Save clip*) |
| Screenshot | **Ctrl+F11** (tray: *Take screenshot*) |
| Open the window | left-click the tray icon, or run `snag.exe` again |
| Start/stop the buffer | tray menu, or the status bar button |
| Rebind hotkeys | Settings tab: click the hotkey, press the new key or mouse button (Esc cancels) |
| Pick what's recorded | Sound tab |
| Open a tab directly | `snag.exe --tab live\|sound\|settings\|quality\|appearance` while Snag runs |

**Hotkeys** can be *any* key, with or without Ctrl/Alt/Shift/Win, including
keys Windows or another program use, and the middle/side mouse buttons:
* A keyboard hook catches every hotkey first and swallows it, so Snag wins
  over other programs and over games that turn off normal app hotkeys.
* Windows' own hotkey registration is added when Windows allows it; it also
  works while a program running *as administrator* is focused, where the
  hook can't see keys. A hotkey only the hook can catch shows a note in
  Settings, a warning when an administrator program takes focus, and a
  **Restart Snag as administrator** button.
* AltGr keyboards (Portuguese, German...) work: a lone Right Alt is Right Alt.
  Punctuation keys show their name on your layout (e.g. `Ctrl+Ç`).
* A plain key (e.g. `F9` or `P`) stops reaching games and other apps while
  Snag runs; a lone modifier (e.g. Right Ctrl) and mouse buttons keep
  working normally.
* Picking the other action's key swaps the two hotkeys.
* Everything is logged in `%LOCALAPPDATA%\Snag\logs\core.log` (keyboard
  layout, whether Snag is elevated, how each hotkey was set up, each press).

**Sound** (Sound tab), recorded as one AAC track (160 kbps by default):
* **Whole PC audio**: every playback device is captured and mixed, so apps
  on different outputs (game on "Game", Discord on "Chat") are all in the
  clip. Untick a device or lower its volume in its strip.
* **Only ticked apps**: per-app capture (Windows 10 2004 or newer), on any
  device. Apps playing sound appear as strips with a live meter; ticks are
  remembered by app and applied immediately (the buffer keeps running).
  On older Windows, the devices those apps play on are captured instead.
* **Microphone**: on by default; pick the device and volume in its strip.

Clips go to `Videos\Snag`, screenshots to `Pictures\Snag` (both
configurable). After a clip is saved, a sound plays and the clip is loaded
into the trimmer, which starts **minimized without taking focus** so a game
in front keeps running. Choose "in front" or "do nothing" in Settings.

**Trimmer keys**: Space play/pause, Left/Right one frame (Shift: one second),
I / O set start / end at the playhead, Home / End jump to the handles.

* *Fast* mode copies the video data (lossless, instant). The start snaps
  back to the previous keyframe (there is one every second).
* *Precise* mode re-encodes, so the cut is frame-exact.

**Clip sound**: `assets\sounds\clip.wav`. Replace it with any WAV. Volume
and on/off are in Settings. A copy of the default sound is built into the
exe as a fallback.

**Settings file**: `%APPDATA%\Snag\settings.json`. Hand edits are picked
up within a second. **Logs**: `%LOCALAPPDATA%\Snag\logs\core.log` and `ui.log`
(set `SNAG_LOG=debug` for more detail). Any dropped frame is logged there
as `DROPPED n frame(s)`.

**Themes**: see [themes/README.md](themes/README.md) for the JSON schema and a full example.

**Branding**: the logo (a play button with a bite taken out; the accent dot
is the clip you snagged) is drawn in code in `src/brand.rs` from the brand
board's geometry, and produces every icon: tray (light or dark to match the
taskbar, dot Ember while recording, grey when stopped), window/taskbar, the
header, and the .exe icon (`build.rs`). The typeface is DM Sans
(Medium + Bold, built into the exe; SIL Open Font License, see
`assets/fonts/DMSans-OFL.txt`).

---

## 3. Architecture

### Two processes

| Process | Contains | Why |
|---|---|---|
| `snag.exe` (core) | tray icon, hotkeys, focus tracking, capture, audio, replay buffer | always running, so it has no window, no GPU UI context, no fonts |
| `snag.exe --ui` | egui window: trimmer, settings, quality, themes | started on demand, exits when closed. An egui window costs ~80 MB, which only matters while you're looking at it |
| `ffmpeg.exe` (sidecar) | H.264 + AAC encoding, segment writing | v1 encoder; swappable (see below) |

The core and UI talk over the UI's stdin/stdout, one JSON message per line
(`src/app/ipc.rs`), so there are no sockets or files to clean up.

### The recording pipeline

```
 Windows Graphics Capture        GPU (Direct3D 11)                     CPU                 ffmpeg
 ------------------------   -------------------------------   --------------------   ------------------------
 frame when screen changes ->  copy -> video processor:      ->  read back NV12   ->  upload to encoder GPU
 (newest only, mailbox)        BGRA -> NV12, scale, letterbox    (1 frame behind,     NVENC/AMF/QSV/x264
                                                                 never stalls)        + AAC audio
                                     ^                                                -> 1 s fMP4 segments
                         pacer thread: exactly `fps` ticks/s                             (rolling, on disk)
 WASAPI loopback (PC audio) -> audio thread: packets placed on the same clock -----^ (named pipe, WAV)
```

* **Capture stays on the GPU** through color conversion and scaling (the
  D3D11 video processor, fixed-function hardware). Only the final
  encoder-sized NV12 image (1.5 bytes/pixel instead of 4) is read back,
  because ffmpeg runs as a separate process.
* **Constant frame rate**: the pacer thread ticks at exactly `fps`, locked
  to the wall clock (tick *n* is at `start + n/fps`). Each tick emits the
  newest frame, or repeats the last one if the screen didn't change. If the
  pacer is ever late, it fills the missed ticks (so timestamps stay
  correct) and **logs them as dropped**.
* **Audio sync**: every captured audio packet carries the time it was
  played (the same performance-counter clock as the video ticks), so it's
  written at exactly the matching position. When nothing plays, Windows
  delivers no audio at all; those stretches are filled with silence so the
  track stays continuous. Audio is delayed by the video's one-frame capture
  latency so sound lines up with the picture.
* **Keyframes**: an IDR keyframe exactly every second, no B-frames, scene-cut
  detection off. Every segment, and therefore every clip, starts on a
  clean frame.
* **Crash-safe buffer**: ffmpeg's HLS muxer writes 1-second fragmented-MP4
  segments to `%LOCALAPPDATA%\Snag\buffer`. A segment only appears under
  its final name when it's complete, so a crash can't corrupt anything
  already written. Old segments are deleted automatically.
* **Saving a clip** (`src/buffer/clip.rs`): wait for the segment containing
  the key press to finish (at most 1 s, so the moment you pressed is
  included), byte-concatenate `init.mp4` + segments, remux to a normal MP4
  with the index at the front (`+faststart`), and rename it into place. It
  never touches the recording pipeline, so there's no hiccup.
* **Monitor switching**: the encoder size is fixed. A new capture session
  starts on the other monitor while the pacer repeats the last frame
  (about 20 ms), and a monitor with a different aspect ratio is letterboxed.
  The stream never restarts, so a clip can span a switch.

### Encoder abstraction

`encode::VideoEncoder` (`src/encode/mod.rs`) is the seam: `prime`, `encode`,
`finish`, `take_audio_sink`, plus a documented on-disk segment contract. To
replace the ffmpeg sidecar with an in-process Media Foundation or NVENC
encoder, implement that trait (ideally taking the NV12 GPU texture
directly, which also removes the readback) and change `create_encoder`.

ffmpeg details that matter (all measured; see comments in `src/encode/ffmpeg.rs`):
1. Color tags are set with a `setparams` filter. Asking for BT.709 on the
   *output* made ffmpeg insert a hidden software color conversion: 3+ CPU
   cores at 1080p60.
2. For NVENC/AMF, frames are uploaded as D3D11 textures (`hwupload`). The
   default path creates a CUDA context and doubles ffmpeg's memory (~410 MB → ~200 MB peak).
3. The encoder is "primed" before the clock starts: ffmpeg needs ~0.2 s to
   load. Without priming, the first ticks were late (5 dropped frames at startup).
4. Audio goes in as a streaming **WAV** over a named pipe, not raw PCM:
   ffmpeg's raw PCM reader works in ~85 ms chunks, and since ffmpeg pauses
   an input that runs ~100 ms ahead of the others, the *video* input kept
   stalling (measured: ~10 dropped frames/s). The WAV reader uses 11 ms packets.

### Module layout

| Module | Contents | Milestone |
|---|---|---|
| `capture/` | monitors, D3D11 device, WGC session, GPU NV12 converter, screenshots | 1 capture |
| `encode/` | `VideoEncoder` trait, ffmpeg sidecar, encoder detection | 1 capture |
| `buffer/` | pacer + recorder + audio thread, frame sources, HLS playlist, clip saving | 2 buffer + clips |
| `audio/` | WASAPI loopback capture, test beep source | audio |
| `hotkeys/`, `app/` | hotkey parsing/registration, keyboard hook; core process (tray, focus hook, controller, IPC) | 3 hotkeys |
| `sound.rs` | clip sound via `PlaySound` (no audio thread while idle) | 3 hotkeys |
| `trimmer/`, `ui/trimmer_tab.rs` | probe, preview decoder, fast/precise export; trimmer UI | 4 trimmer |
| `themes/`, `ui/appearance_tab.rs` | JSON themes, egui styling, hot reload | 5 themes |
| `tune/`, `ui/quality_tab.rs` | hardware detection, presets, recommendation | 6 auto-tune |
| `config/` | settings struct, load/save (atomic, BOM-tolerant) | all |
| `verify.rs` | barcode test frames, clip verification (video + audio sync), live test-pattern window | tests |
| `paths.rs`, `logging.rs`, `win.rs` | file locations, file logger, Win32 helpers | all |

### Why these choices (short)

* **Rust + windows-rs**: direct access to WGC/D3D11/WASAPI/Win32 with no runtime and no GC pauses.
* **egui** (OpenGL backend): native, small, immediate-mode. The whole UI is plain Rust code, with no markup or webview.
* **RegisterHotKey first, keyboard hook only when needed**: zero
  per-keystroke cost in the common case; the hook makes any key possible.
* **PlaySound from memory** for the clip chime: no audio stream kept open, so idle cost is zero.
* **Disk segments** instead of a RAM ring buffer: keeps memory flat (a 30 s
  buffer at 18.5 Mbps would be ~70 MB of RAM) and survives crashes.

---

## 4. Known limitations

1. **Audio is the whole PC mix** (game + voice chat + music). There's no
   microphone track and no per-game audio yet. The first 0.5 s after the
   buffer (re)starts is silent.
2. **Hook-caught hotkeys** (keys Windows wouldn't register) don't fire while
   an administrator program is focused, unless Snag also runs as administrator.
3. **One GPU→CPU→GPU round trip per frame** because the encoder is a separate
   ffmpeg process (~190 MB/s of NV12 at 1080p60). Cheap on desktop GPUs; an
   in-process encoder would remove it.
4. **Windows 10 and Windows 11 before 24H2** cap capture delivery at ~60 fps
   (`MinUpdateInterval` is unavailable), so at 60 fps recording an occasional
   frame can be repeated when a game renders faster than 60 fps.
5. **When the whole CPU is saturated** (all cores at 100 %), the Windows
   compositor itself can skip frames. Snag then repeats the last frame,
   which is correct (no newer image exists).
6. **Protected content and old exclusive-fullscreen games** (some DX9 titles)
   may capture black: a Windows Graphics Capture restriction.
7. **HDR** monitors are captured as 8-bit SDR. Output is H.264 only (no HEVC/AV1).
8. **AMD AMF and Intel Quick Sync** are implemented, but were only checked
   against ffmpeg's option list on the NVIDIA development PC. The startup
   probe disables any encoder that fails a real test encode, so they can't
   break recording; they just fall back.
9. **Clip length** is rounded up to whole seconds (a 30 s clip is 30–31 s),
   and saving waits up to 1 s for the segment holding the key press.
10. **Changing video or audio settings restarts the buffer** (its contents are discarded).
11. **Trimmer preview** is software-decoded at up to 960x540 and plays
    **without sound** (the exported clip keeps its audio); seeking far away
    restarts the decoder (~0.1 s).
12. Not code-signed, so SmartScreen warns on first run. No installer, no
    start-with-Windows option yet.

## 5. What to build next

1. **Microphone track** (separate from game audio) and per-app audio (Windows 10 2004+ process loopback).
2. **Audio in the trimmer preview**.
3. **In-process encoder** (Media Foundation H.264/HEVC with D3D11 texture
   input, or the NVENC/AMF SDKs): no readback, no ffmpeg process (saves
   ~55 MB and ~1 % CPU), and lower latency.
4. **HEVC/AV1 and HDR** capture (FP16 surfaces + PQ/BT.2020).
5. **Trimmer polish**: thumbnail filmstrip, hardware-decoded preview, clip rename/delete.
6. **Start with Windows**, toast notifications ("Clip saved, open?"), per-game profiles, window-only capture, mouse-button hotkeys.
7. **Installer** (MSIX or WiX), code signing, auto-update.

---

## Troubleshooting

* **"Snag needs ffmpeg"**: run `winget install Gyan.FFmpeg`, then restart
  Snag. Or run `scripts\bundle-ffmpeg.ps1`, or put `ffmpeg.exe` and
  `ffprobe.exe` in a `bin` folder next to `snag.exe`.
* **A hotkey doesn't fire in one game**: that game probably runs as
  administrator (see limitation 2). Pick a key Windows can register (with
  Ctrl/Alt/Shift), or run Snag as administrator.
* **No sound in clips**: open the Sound tab. "Recording now" lists what
  is captured; in "only ticked apps" mode, tick the game/app.
* **Black clips**: see limitation 6. Try borderless-windowed mode in the game.
* **Dropped frames in the log**: lower the preset in the Quality tab (Balanced or Performance).
