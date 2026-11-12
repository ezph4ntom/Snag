//! Native Windows file/folder pickers (IFileOpenDialog), run on their own
//! thread so the egui window keeps repainting while a dialog is open.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};

use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName,
    SIGDN_FILESYSPATH,
};
use windows::core::{HSTRING, w};

#[derive(Clone, Copy)]
enum Kind {
    Folder,
    Video,
    Audio,
}

fn show(initial: &Path, kind: Kind) -> Option<PathBuf> {
    let folders = matches!(kind, Kind::Folder);
    // SAFETY: standard COM usage on a fresh thread with its own apartment.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let result = (|| -> windows::core::Result<PathBuf> {
            let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
            let mut opts = dialog.GetOptions()? | FOS_FORCEFILESYSTEM;
            if folders {
                opts |= FOS_PICKFOLDERS;
            } else {
                let first = match kind {
                    Kind::Audio => COMDLG_FILTERSPEC {
                        pszName: w!("Sounds"),
                        pszSpec: w!("*.wav;*.mp3;*.ogg;*.flac;*.m4a;*.aac;*.opus;*.wma"),
                    },
                    _ => COMDLG_FILTERSPEC { pszName: w!("Videos"), pszSpec: w!("*.mp4;*.mkv;*.mov;*.m4v") },
                };
                dialog.SetFileTypes(&[first, COMDLG_FILTERSPEC { pszName: w!("All files"), pszSpec: w!("*.*") }])?;
            }
            dialog.SetOptions(opts)?;
            if initial.is_dir() {
                if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(initial.as_os_str()), None) {
                    let _ = dialog.SetFolder(&item);
                }
            }
            dialog.Show(None)?; // Err when cancelled
            let item = dialog.GetResult()?;
            let name = item.GetDisplayName(SIGDN_FILESYSPATH)?;
            let path = PathBuf::from(name.to_string().unwrap_or_default());
            CoTaskMemFree(Some(name.0 as *const _));
            Ok(path)
        })();
        CoUninitialize();
        result.ok()
    }
}

/// Opens a folder picker; the result arrives on the returned channel
/// (nothing is sent if the user cancels).
pub fn pick_folder(initial: &Path) -> Receiver<PathBuf> {
    spawn(initial, Kind::Folder)
}

pub fn pick_video(initial: &Path) -> Receiver<PathBuf> {
    spawn(initial, Kind::Video)
}

pub fn pick_sound(initial: &Path) -> Receiver<PathBuf> {
    spawn(initial, Kind::Audio)
}

fn spawn(initial: &Path, kind: Kind) -> Receiver<PathBuf> {
    let (tx, rx) = channel();
    let initial = initial.to_path_buf();
    std::thread::spawn(move || {
        if let Some(p) = show(&initial, kind) {
            let _ = tx.send(p);
        }
    });
    rx
}
