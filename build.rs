//! Build script: gives snag.exe its icon and version info.
//!
//! The icon is generated from the same logo code the app uses at runtime
//! (`src/brand.rs`), written as an .ico into Cargo's build folder, and
//! compiled into the exe with the Windows resource compiler (rc.exe, part of
//! the Windows SDK). The version block is what Explorer's Properties and
//! Task Manager show ("Snag").
//!
//! If the resource compiler can't be found, the build still succeeds; the
//! exe just has Windows' default icon.

#[path = "src/brand.rs"]
#[allow(dead_code)]
mod brand;

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=src/brand.rs");
    println!("cargo:rerun-if-changed=build.rs");
    build_info();

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let ico = out.join("snag.ico");
    let sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256];
    std::fs::write(&ico, brand::app_icon_ico(&sizes, brand::EMBER)).expect("write icon");

    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let nums: Vec<u16> = version.split('.').map(|p| p.parse().unwrap_or(0)).collect();
    let (a, b, c) = (nums.first().copied().unwrap_or(0), nums.get(1).copied().unwrap_or(0), nums.get(2).copied().unwrap_or(0));
    let rc = format!(
        r#"1 ICON "{icon}"

1 VERSIONINFO
FILEVERSION {a},{b},{c},0
PRODUCTVERSION {a},{b},{c},0
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "ProductName", "Snag"
      VALUE "FileDescription", "Snag - game clipping"
      VALUE "OriginalFilename", "snag.exe"
      VALUE "InternalName", "snag"
      VALUE "FileVersion", "{version}"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        icon = ico.display().to_string().replace('\\', "\\\\"),
    );
    let rc_path = out.join("snag.rc");
    std::fs::write(&rc_path, rc).expect("write rc");
    if let Err(e) = embed_resource::compile(&rc_path, embed_resource::NONE).manifest_optional() {
        println!("cargo:warning=exe icon not embedded ({e}); install the Windows SDK to get it");
    }
}

/// The commit and its date, shown in Settings > About
/// (SNAG_BUILD_COMMIT = "0df40a3", SNAG_BUILD_DATE = "17 Nov 2026").
fn build_info() {
    // Re-run when a commit is made or the branch changes.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let commit = git(&["rev-parse", "--short=7", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let date = git(&["log", "-1", "--format=%cs"])
        .and_then(|d| {
            // "2026-11-17" -> "17 Nov 2026"
            let mut p = d.split('-');
            let (y, m, day) = (p.next()?, p.next()?.parse::<usize>().ok()?, p.next()?.parse::<u32>().ok()?);
            let months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
            Some(format!("{day} {} {y}", months.get(m.checked_sub(1)?)?))
        })
        .unwrap_or_else(|| "unknown date".into());
    println!("cargo:rustc-env=SNAG_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=SNAG_BUILD_DATE={date}");
}
