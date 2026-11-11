# Testing ClipApp: quick guide

Thanks for testing! This takes about 5 minutes to set up.

## 1. Install

1. **Install ffmpeg** (the video encoder ClipApp uses). Open *PowerShell*
   or *Terminal* and run:

   ```
   winget install Gyan.FFmpeg
   ```

2. **Download** `ClipApp-v….zip` from the repository's **Releases** page
   (right side of the repo page) and unzip it anywhere, e.g. your Desktop.
3. **Run** `ClipApp\clipapp.exe`.
   * Windows may show *"Windows protected your PC"* because the app isn't
     code-signed. Click **More info → Run anyway**.
   * Nothing opens except a **tray icon** (bottom-right, maybe behind the `^`
     arrow) and, the first time, a window showing what quality settings
     were picked for your PC and why.

Requirements: Windows 10 (version 1903+) or Windows 11, any GPU with up-to-date drivers.

## 2. Use it

| Do this | Result |
|---|---|
| Play a game for a minute, press **Ctrl+F10** | The last 30 seconds (video + PC sound) are saved to `Videos\ClipApp`, with a chime |
| Press **Ctrl+F11** | A screenshot is saved to `Pictures\ClipApp` |
| Left-click the tray icon | Opens the window: clips + trimmer, settings, quality, themes |
| Right-click the tray icon | Start/stop recording, open the clips folder, quit |

Hotkeys can be changed in **Settings**: click a hotkey and press **any**
key or combination (Esc cancels).

## 3. Things worth testing

- [ ] Clips from a fullscreen game and a borderless-window game look right (no black, frozen or glitched frames)
- [ ] Game sound is in the clip and in sync with the picture
- [ ] With two monitors: Alt-Tab to the other monitor, then save a clip (capture should follow you)
- [ ] The trimmer: drag the handles, step frames with the arrow keys, save in *Fast* and *Precise* mode
- [ ] Rebinding a hotkey to an unusual key (a mouse-less key like Print Screen, F12, or Right Ctrl)
- [ ] Does your game's FPS drop noticeably while ClipApp runs? (Try the Performance preset in the Quality tab if so.)
- [ ] Switching themes in the Appearance tab

## 4. Send feedback

Open an **Issue** in the repository (Issues tab → New issue) and pick
*Bug report* or *Feedback*. For bugs, please attach:

1. The log files: press `Win+R`, paste `%LOCALAPPDATA%\ClipApp\logs`, press
   Enter, and attach `core.log` (and `ui.log` if the window misbehaved).
2. Your hardware detection: in the window's **Quality** tab, or run
   `clipapp.exe --detect` in a terminal and paste the output.
3. What you did, what you expected, what happened. A short clip of the problem helps a lot.

To quit ClipApp: right-click the tray icon → **Quit ClipApp**. To uninstall:
delete the folder, plus `%APPDATA%\ClipApp` (settings) and
`%LOCALAPPDATA%\ClipApp` (logs).
