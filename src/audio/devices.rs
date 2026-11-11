//! What's playing where: playback devices, the apps with audio sessions on
//! them, and their live levels.
//!
//! Windows keeps an "audio session" per app per playback device (the rows of
//! the volume mixer). Enumerating them tells us which apps make sound and on
//! which device; each session and device also has a peak meter, which the
//! Sound tab shows. Nothing here captures audio.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::{
    AudioSessionStateActive, AudioSessionStateExpired, DEVICE_STATE_ACTIVE, IAudioSessionControl2,
    IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, STGM_READ};
use windows::core::Interface;

/// A playback device ("Speakers (HyperX Cloud Alpha S Game)") or a
/// microphone.
#[derive(Clone, Debug)]
pub struct RenderDevice {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// One app's audio session on one device.
pub struct Session {
    pub pid: u32,
    pub device_id: String,
    /// Currently playing (not just opened and paused).
    pub active: bool,
    pub meter: Option<IAudioMeterInformation>,
}

fn enumerator() -> Result<IMMDeviceEnumerator> {
    // SAFETY: standard COM activation (the calling thread initialized COM).
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).context("audio device enumerator") }
}

fn device_id(d: &IMMDevice) -> String {
    // SAFETY: the returned string is ours to free.
    unsafe {
        d.GetId()
            .map(|p| {
                let s = p.to_string().unwrap_or_default();
                CoTaskMemFree(Some(p.0 as *const _));
                s
            })
            .unwrap_or_default()
    }
}

fn device_name(d: &IMMDevice) -> String {
    // SAFETY: property store read; the PROPVARIANT holds a string we copy.
    unsafe {
        let Ok(store) = d.OpenPropertyStore(STGM_READ) else { return String::new() };
        let Ok(value) = store.GetValue(&PKEY_Device_FriendlyName) else { return String::new() };
        let p = value.Anonymous.Anonymous.Anonymous.pwszVal;
        if p.is_null() { String::new() } else { p.to_string().unwrap_or_default() }
    }
}

/// Active playback devices (plugged in and enabled).
pub fn render_devices() -> Result<Vec<RenderDevice>> {
    endpoints(eRender)
}

/// Active microphones / input devices.
pub fn capture_devices() -> Result<Vec<RenderDevice>> {
    endpoints(windows::Win32::Media::Audio::eCapture)
}

fn endpoints(flow: windows::Win32::Media::Audio::EDataFlow) -> Result<Vec<RenderDevice>> {
    let e = enumerator()?;
    // SAFETY: COM calls on valid interfaces.
    unsafe {
        let default = e.GetDefaultAudioEndpoint(flow, eConsole).map(|d| device_id(&d)).unwrap_or_default();
        let coll = e.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
        let mut out = Vec::new();
        for i in 0..coll.GetCount()? {
            let d = coll.Item(i)?;
            let id = device_id(&d);
            out.push(RenderDevice { is_default: id == default, name: device_name(&d), id });
        }
        Ok(out)
    }
}

/// The device's peak meter (its overall output level).
pub fn device_meter(id: &str) -> Option<IAudioMeterInformation> {
    let wide = crate::win::to_wide(id);
    // SAFETY: COM calls on valid interfaces.
    unsafe {
        let d = enumerator().ok()?.GetDevice(windows::core::PCWSTR(wide.as_ptr())).ok()?;
        d.Activate(CLSCTX_ALL, None).ok()
    }
}

/// Every app audio session on every active playback device (except
/// Windows' system sounds and Snag itself).
pub fn sessions() -> Vec<Session> {
    let mut out = Vec::new();
    let me = std::process::id();
    let Ok(e) = enumerator() else { return out };
    // SAFETY: COM calls on valid interfaces.
    unsafe {
        let Ok(coll) = e.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else { return out };
        for i in 0..coll.GetCount().unwrap_or(0) {
            let Ok(d) = coll.Item(i) else { continue };
            let id = device_id(&d);
            let Ok(mgr) = d.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else { continue };
            let Ok(list) = mgr.GetSessionEnumerator() else { continue };
            for j in 0..list.GetCount().unwrap_or(0) {
                let Ok(ctl) = list.GetSession(j) else { continue };
                let Ok(ctl2) = ctl.cast::<IAudioSessionControl2>() else { continue };
                if ctl2.IsSystemSoundsSession().0 == 0 {
                    continue; // S_OK means "yes, system sounds"
                }
                let pid = ctl2.GetProcessId().unwrap_or(0);
                let state = ctl.GetState().unwrap_or(AudioSessionStateExpired);
                if pid == 0 || pid == me || state == AudioSessionStateExpired {
                    continue;
                }
                out.push(Session {
                    pid,
                    device_id: id.clone(),
                    active: state == AudioSessionStateActive,
                    meter: ctl.cast::<IAudioMeterInformation>().ok(),
                });
            }
        }
    }
    out
}

pub fn peak(meter: &IAudioMeterInformation) -> f32 {
    // SAFETY: valid meter interface.
    unsafe { meter.GetPeakValue().unwrap_or(0.0) }
}

/// Parent process of every running process (for "is this process inside
/// that one's tree").
pub fn parent_pids() -> HashMap<u32, u32> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    };
    let mut map = HashMap::new();
    // SAFETY: snapshot walk with a correctly sized entry; handle closed.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return map };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                map.insert(e.th32ProcessID, e.th32ParentProcessID);
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    map
}

/// True if `ancestor` is a parent, grandparent... of `pid`.
pub fn is_descendant(pid: u32, ancestor: u32, parents: &HashMap<u32, u32>) -> bool {
    let mut p = pid;
    for _ in 0..32 {
        match parents.get(&p) {
            Some(&pp) if pp == ancestor => return true,
            Some(&pp) if pp != 0 && pp != p => p = pp,
            _ => return false,
        }
    }
    false
}

/// A readable app name: the exe's "File description" ("Google Chrome",
/// "Spotify"), or the file name without `.exe`.
pub fn app_display_name(exe_path: &Path) -> String {
    use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
    let stem = exe_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let wide = crate::win::to_wide(&exe_path.to_string_lossy());
    // SAFETY: version-info buffer sized by the first call; the strings we
    // read point into it.
    unsafe {
        let size = GetFileVersionInfoSizeW(windows::core::PCWSTR(wide.as_ptr()), None);
        if size == 0 {
            return stem;
        }
        let mut data = vec![0u8; size as usize];
        if GetFileVersionInfoW(windows::core::PCWSTR(wide.as_ptr()), None, size, data.as_mut_ptr() as *mut _).is_err() {
            return stem;
        }
        // The first language/codepage pair of the translation table.
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        let key = crate::win::to_wide("\\VarFileInfo\\Translation");
        if !VerQueryValueW(data.as_ptr() as *const _, windows::core::PCWSTR(key.as_ptr()), &mut ptr, &mut len).as_bool()
            || len < 4
        {
            return stem;
        }
        let pair = std::slice::from_raw_parts(ptr as *const u16, 2);
        let key = crate::win::to_wide(&format!("\\StringFileInfo\\{:04x}{:04x}\\FileDescription", pair[0], pair[1]));
        if !VerQueryValueW(data.as_ptr() as *const _, windows::core::PCWSTR(key.as_ptr()), &mut ptr, &mut len).as_bool()
            || len == 0
        {
            return stem;
        }
        let s = std::slice::from_raw_parts(ptr as *const u16, len as usize);
        let name = String::from_utf16_lossy(s).trim_matches(char::from(0)).trim().to_string();
        if name.is_empty() || name.len() > 48 { stem } else { name }
    }
}

/// Games mostly live in a launcher's library folder.
pub fn looks_like_game(exe_path: &Path) -> bool {
    let p = exe_path.to_string_lossy().to_ascii_lowercase();
    [
        "\\steamapps\\common\\",
        "\\epic games\\",
        "\\riot games\\",
        "\\battle.net\\",
        "\\ubisoft game launcher\\games\\",
        "\\ea games\\",
        "\\gog galaxy\\games\\",
        "\\xboxgames\\",
        "\\rockstar games\\",
        "\\games\\",
    ]
    .iter()
    .any(|k| p.contains(k))
}

/// Lower-case exe file name, the key apps are remembered by.
pub fn exe_key(exe_path: &Path) -> String {
    exe_path.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_tree() {
        let parents: HashMap<u32, u32> = [(10, 1), (11, 10), (12, 11), (20, 1)].into();
        assert!(is_descendant(12, 10, &parents));
        assert!(is_descendant(11, 10, &parents));
        assert!(!is_descendant(20, 10, &parents));
        assert!(!is_descendant(10, 12, &parents));
    }

    #[test]
    fn game_paths() {
        assert!(looks_like_game(Path::new(r"D:\SteamLibrary\steamapps\common\Game\game.exe")));
        assert!(!looks_like_game(Path::new(r"C:\Users\x\AppData\Roaming\Spotify\Spotify.exe")));
        assert_eq!(exe_key(Path::new(r"C:\a\Discord.exe")), "discord.exe");
    }
}
