//! PNG screenshots of a monitor.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Win32::Graphics::Direct3D11::{D3D11_TEXTURE2D_DESC, ID3D11Texture2D};
use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;
use windows::core::Interface;

use super::convert::Nv12Converter;
use super::d3d::D3D;
use super::wgc::WgcCapture;

/// A CPU-side image in BGRA order (what the GPU gives us).
pub struct BgraImage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Writes a BGRA image as a 24-bit RGB PNG (alpha is meaningless for a
/// screenshot and dropping it makes files ~25% smaller).
pub fn write_png(img: &BgraImage, path: &Path) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut rgb = Vec::with_capacity((img.width * img.height * 3) as usize);
    for px in img.data.as_chunks::<4>().0 {
        rgb.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    // Write to a temp name and rename, so a half-written PNG never appears.
    let tmp = path.with_extension("png.part");
    {
        let file = std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), img.width, img.height);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        // "Fast" deflate: a 1080p screenshot encodes in ~30 ms instead of ~300.
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc.write_header()?;
        writer.write_image_data(&rgb)?;
        writer.finish()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Saves `img` as `Screenshot_<timestamp>.png` in `dir`.
pub fn save_screenshot(img: &BgraImage, dir: &Path) -> Result<PathBuf> {
    let path = crate::win::unique_path(dir, &format!("Screenshot_{}", crate::win::file_timestamp()), "png");
    write_png(img, &path)?;
    Ok(path)
}

/// Gets the D3D11 texture behind a WGC frame.
pub fn frame_texture(
    frame: &windows::Graphics::Capture::Direct3D11CaptureFrame,
) -> Result<(ID3D11Texture2D, u32, u32)> {
    let surface = frame.Surface()?;
    let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
    // SAFETY: WGC surfaces are always backed by ID3D11Texture2D.
    let tex: ID3D11Texture2D = unsafe { access.GetInterface()? };
    let size = frame.ContentSize()?;
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: valid texture, desc is an out-param.
    unsafe { tex.GetDesc(&mut desc) };
    // ContentSize can briefly exceed the buffer during a resolution change.
    let w = (size.Width.max(1) as u32).min(desc.Width);
    let h = (size.Height.max(1) as u32).min(desc.Height);
    Ok((tex, w, h))
}

/// One-shot capture for when the replay buffer isn't running: starts a WGC
/// session, waits for the first frame, copies it out, stops.
pub fn capture_once(monitor: isize, capture_cursor: bool) -> Result<BgraImage> {
    let d3d = D3D::new()?;
    let cap = WgcCapture::start(&d3d, monitor, capture_cursor)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(frame) = cap.take_latest() {
            let (tex, w, h) = frame_texture(&frame)?;
            // Reuse the converter's BGRA readback path; the NV12 side is unused.
            let mut conv = Nv12Converter::new(&d3d, w, h, 64, 64)?;
            conv.submit(&tex, w, h)?;
            let _ = frame.Close();
            return conv.read_input_bgra();
        }
        if Instant::now() > deadline {
            bail!("no frame from the screen within 2 s");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
