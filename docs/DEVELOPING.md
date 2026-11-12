# Developing Snag

Everything technical: building, packaging, tests, and how Snag works inside.
For using Snag, see the [README](../README.md); for testers, [TESTING.md](../TESTING.md).

## Build and run

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
cd C:\Snag   # wherever you cloned the repository

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
cargo test --release --test trimmer -- --ignored --nocapture       # trim exactness + preview sound (~10 s)
cargo test --release --test recording -- --ignored --nocapture     # long recordings: snapshot + stop (~15 s)
cargo test --release --test audio_capture -- --ignored --nocapture # apps on two devices, mic (plays quiet tones)
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


## Architecture

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


## Known limitations

1. **Hook-only hotkeys** (keys Windows won't register, lone modifiers,
   mouse buttons) don't fire while a program running as administrator is
   focused. Settings shows a warning and offers *Restart Snag as
   administrator*.
2. **Per-app audio** needs Windows 10 version 2004 or newer; older systems
   record the whole devices those apps play on.
3. **One GPU→CPU→GPU round trip per frame**, because the encoder is a
   separate ffmpeg process (~190 MB/s of NV12 at 1080p60). An in-process
   encoder would remove it.
4. **Windows 10 and Windows 11 before 24H2** deliver captured frames at
   ~60 fps at most, so an occasional frame can repeat when a game renders
   faster.
5. **Protected content and old exclusive-fullscreen games** may capture
   black (a Windows Graphics Capture restriction). Borderless windowed works.
6. **HDR** is captured as 8-bit SDR; output is H.264 only.
7. **AMD AMF and Intel Quick Sync** were only checked against ffmpeg's
   option list on the NVIDIA development PC. A startup test encode disables
   any encoder that fails, so they fall back rather than break.
8. **Changing video settings restarts the buffer** (its contents are
   discarded; a running recording is saved first).
9. **Long recordings** are remuxed into a normal MP4 when saved: that takes
   a few seconds per GB and briefly needs twice the file's size on disk.
10. Not code-signed, so SmartScreen warns on first run.

## Ideas for later

* In-process encoder (Media Foundation or the NVENC/AMF SDKs): no readback,
  no ffmpeg process.
* HEVC/AV1 and HDR capture.
* Trimmer: thumbnail filmstrip, hardware-decoded preview, rename/delete.
* Per-game profiles, window-only capture, auto-update, code signing.
