//! Auto-tuned quality: detect the hardware once, derive three presets from
//! it, recommend one, and explain every choice in plain words.

use serde::{Deserialize, Serialize};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIFactory1};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

use crate::config::VideoSettings;
use crate::encode::EncoderKind;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct GpuInfo {
    pub name: String,
    pub vendor: String,
    pub vram_mb: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct MonitorSummary {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub primary: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct HardwareInfo {
    /// Detection format version; older results are re-detected on startup.
    pub version: u32,
    pub gpus: Vec<GpuInfo>,
    /// Encoders that passed a real test encode, best first.
    pub encoders: Vec<EncoderKind>,
    /// Encoders whose lighter D3D11 frame-upload path also passed.
    pub gpu_upload: Vec<EncoderKind>,
    pub cpu_threads: u32,
    pub ram_gb: f32,
    pub monitors: Vec<MonitorSummary>,
    pub ffmpeg: String,
    pub detected_at: String,
}

/// Bump when detection learns something new, to re-detect old results.
pub const DETECT_VERSION: u32 = 2;

impl HardwareInfo {
    pub fn is_current(&self) -> bool {
        self.version >= DETECT_VERSION
    }

    pub fn primary_monitor(&self) -> (u32, u32, u32) {
        self.monitors
            .iter()
            .find(|m| m.primary)
            .or(self.monitors.first())
            .map(|m| (m.width, m.height, m.refresh_hz))
            .unwrap_or((1920, 1080, 60))
    }

    pub fn best_encoder(&self) -> EncoderKind {
        self.encoders.first().copied().unwrap_or(EncoderKind::X264)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Preset {
    Quality,
    Balanced,
    Performance,
    /// The user changed something by hand.
    Custom,
}

impl Preset {
    pub const AUTO: [Preset; 3] = [Preset::Quality, Preset::Balanced, Preset::Performance];

    pub fn label(self) -> &'static str {
        match self {
            Preset::Quality => "Quality",
            Preset::Balanced => "Balanced",
            Preset::Performance => "Performance",
            Preset::Custom => "Custom",
        }
    }
}

/// Concrete video settings a preset resolves to.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub preset: Preset,
    pub encoder: EncoderKind,
    pub output_height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// One line per decision, shown in the UI.
    pub why: Vec<String>,
}

impl Plan {
    pub fn apply_to(&self, v: &mut VideoSettings) {
        v.preset = self.preset;
        v.encoder = self.encoder;
        v.output_height = self.output_height;
        v.fps = self.fps;
        v.bitrate_kbps = self.bitrate_kbps;
    }
}

pub struct Recommendation {
    pub preset: Preset,
    pub why: Vec<String>,
}

/// Runs all detection. Takes about a second (mostly the encoder probes).
pub fn detect() -> HardwareInfo {
    let monitors = crate::capture::enumerate_monitors()
        .into_iter()
        .map(|m| MonitorSummary {
            name: m.device_name.trim_start_matches(r"\\.\").to_string(),
            width: m.width,
            height: m.height,
            refresh_hz: m.refresh_hz,
            primary: m.primary,
        })
        .collect();
    let probes = crate::encode::detect::probe_all();
    let info = HardwareInfo {
        version: DETECT_VERSION,
        gpus: detect_gpus(),
        encoders: probes.iter().filter(|p| p.works).map(|p| p.kind).collect(),
        gpu_upload: probes.iter().filter(|p| p.gpu_upload).map(|p| p.kind).collect(),
        cpu_threads: std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(4),
        ram_gb: detect_ram_gb(),
        monitors,
        ffmpeg: crate::encode::detect::ffmpeg_version().unwrap_or_else(|e| format!("not found: {e}")),
        detected_at: crate::win::file_timestamp(),
    };
    log::info!(
        "hardware: GPUs {:?}, encoders {:?}, {} threads, {:.1} GB RAM, monitors {:?}",
        info.gpus.iter().map(|g| &g.name).collect::<Vec<_>>(),
        info.encoders,
        info.cpu_threads,
        info.ram_gb,
        info.monitors.iter().map(|m| format!("{}x{}@{}", m.width, m.height, m.refresh_hz)).collect::<Vec<_>>()
    );
    info
}

/// GPUs via DXGI (cheap: no encoder probing).
pub fn detect_gpus() -> Vec<GpuInfo> {
    let mut out = Vec::new();
    // SAFETY: standard DXGI enumeration; EnumAdapters1 errors past the end.
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return out;
        };
        let mut i = 0;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            i += 1;
            let Ok(desc) = adapter.GetDesc1() else { continue };
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                continue; // "Microsoft Basic Render Driver"
            }
            let len = desc.Description.iter().position(|&c| c == 0).unwrap_or(128);
            out.push(GpuInfo {
                name: String::from_utf16_lossy(&desc.Description[..len]),
                vendor: match desc.VendorId {
                    0x10DE => "NVIDIA",
                    0x1002 | 0x1022 => "AMD",
                    0x8086 => "Intel",
                    _ => "Other",
                }
                .into(),
                vram_mb: desc.DedicatedVideoMemory as u64 / (1024 * 1024),
            });
        }
    }
    out
}

fn detect_ram_gb() -> f32 {
    let mut m = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: dwLength is set as the API requires.
    if unsafe { GlobalMemoryStatusEx(&mut m) }.is_ok() {
        m.ullTotalPhys as f32 / (1024.0 * 1024.0 * 1024.0)
    } else {
        8.0
    }
}

/// Width for a given output height, keeping the monitor's aspect ratio.
pub fn width_for(native_w: u32, native_h: u32, h: u32) -> u32 {
    ((native_w as u64 * h as u64 / native_h.max(1) as u64) as u32).max(2) & !1
}

/// Bitrate from a "bits per pixel per frame" budget, rounded to 500 kbps.
fn bitrate(w: u32, h: u32, fps: u32, bpp: f64) -> u32 {
    let kbps = w as f64 * h as f64 * fps as f64 * bpp / 1000.0;
    ((kbps / 500.0).round() as u32 * 500).clamp(2500, 80_000)
}

/// What `preset` means on this hardware.
pub fn plan(hw: &HardwareInfo, preset: Preset) -> Plan {
    let (nw, nh, refresh) = hw.primary_monitor();
    let encoder = hw.best_encoder();
    let hardware = encoder.is_hardware();
    let mut why = Vec::new();

    why.push(if hardware {
        format!(
            "Encoder: {}. Encoding runs on a separate chip on the GPU, so it barely affects game FPS.",
            encoder.label()
        )
    } else {
        "Encoder: x264 on the CPU, because no working hardware encoder was found. It uses CPU time the game could use, so settings are kept modest.".to_string()
    });

    // (height cap, fps, bits-per-pixel) per preset.
    let (cap, fps, bpp) = match (preset, hardware) {
        (Preset::Quality | Preset::Custom, true) => (1440, 60, 0.15),
        (Preset::Balanced, true) => (1080, 60, 0.10),
        (Preset::Performance, true) => (720, 60, 0.09),
        (Preset::Quality | Preset::Custom, false) => (1080, if hw.cpu_threads >= 12 { 60 } else { 30 }, 0.12),
        (Preset::Balanced, false) => (1080, 30, 0.10),
        (Preset::Performance, false) => (720, 30, 0.10),
    };
    let fps = fps.min(refresh.max(30));
    let h = nh.min(cap) & !1;
    let w = width_for(nw, nh, h);
    let output_height = if h >= nh { 0 } else { h };

    why.push(if output_height == 0 {
        format!("Resolution: {w}x{h}, your monitor's native resolution.")
    } else {
        format!(
            "Resolution: {w}x{h}, scaled down from {nw}x{nh} on the GPU. Fewer pixels means less to copy and encode each frame."
        )
    });
    why.push(if refresh > fps {
        format!(
            "Frame rate: {fps} fps (your monitor runs at {refresh} Hz). {fps} fps keeps clips smooth; higher rates multiply encoder and disk load for little visible gain in shared clips."
        )
    } else {
        format!("Frame rate: {fps} fps, matching your monitor.")
    });
    let kbps = bitrate(w, h, fps, bpp);
    why.push(format!(
        "Bitrate: {:.1} Mbps (about {bpp:.2} bits per pixel per frame), {}. A 30 s clip is about {} MB.",
        kbps as f64 / 1000.0,
        match preset {
            Preset::Quality | Preset::Custom => "near-transparent quality for fast motion",
            Preset::Balanced => "clean for most games with smaller files than Quality",
            Preset::Performance => "smallest files and lowest load",
        },
        kbps as u64 * 30 / 8 / 1000
    ));
    Plan {
        preset,
        encoder,
        output_height,
        fps,
        bitrate_kbps: kbps,
        why,
    }
}

/// Picks a preset for this machine and says why.
pub fn recommend(hw: &HardwareInfo) -> Recommendation {
    let (_, nh, _) = hw.primary_monitor();
    let enc = hw.best_encoder();
    let specs = format!("{} CPU threads, {:.0} GB RAM", hw.cpu_threads, hw.ram_gb);
    let (preset, reason) = if !enc.is_hardware() {
        (
            Preset::Performance,
            "No hardware encoder: video is encoded on the CPU, so Performance keeps that cost away from your game.".to_string(),
        )
    } else if hw.ram_gb < 7.5 {
        (Preset::Balanced, format!("Less than 8 GB RAM ({specs}): Balanced keeps memory and disk use moderate."))
    } else if enc == EncoderKind::Qsv {
        (
            Preset::Balanced,
            "Intel Quick Sync runs on the integrated GPU, which some games also use; Balanced leaves it headroom.".to_string(),
        )
    } else if nh > 1440 {
        (
            Preset::Balanced,
            format!(
                "Your monitor is {nh}p. Capturing at that size means copying over twice the pixels of 1080p every frame; Balanced (1080p) keeps the GPU cost low."
            ),
        )
    } else {
        (
            Preset::Quality,
            format!(
                "{} is a dedicated encoder and you have {specs}: full quality costs almost nothing in game FPS.",
                enc.label()
            ),
        )
    };
    Recommendation {
        preset,
        why: vec![reason],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hw(encoders: Vec<EncoderKind>, h: u32) -> HardwareInfo {
        HardwareInfo {
            encoders,
            cpu_threads: 16,
            ram_gb: 16.0,
            monitors: vec![MonitorSummary {
                name: "DISPLAY1".into(),
                width: h * 16 / 9,
                height: h,
                refresh_hz: 144,
                primary: true,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn nvenc_1080p_recommends_quality_at_native() {
        let hw = hw(vec![EncoderKind::Nvenc, EncoderKind::X264], 1080);
        assert_eq!(recommend(&hw).preset, Preset::Quality);
        let p = plan(&hw, Preset::Quality);
        assert_eq!((p.output_height, p.fps, p.encoder), (0, 60, EncoderKind::Nvenc));
        assert!(p.bitrate_kbps > 15_000 && p.bitrate_kbps < 25_000);
    }

    #[test]
    fn software_only_recommends_performance() {
        let hw = hw(vec![EncoderKind::X264], 1080);
        assert_eq!(recommend(&hw).preset, Preset::Performance);
        let p = plan(&hw, Preset::Performance);
        assert_eq!((p.output_height, p.fps), (720, 30));
    }

    #[test]
    fn four_k_is_scaled_and_even() {
        let hw = hw(vec![EncoderKind::Nvenc], 2160);
        let p = plan(&hw, Preset::Quality);
        assert_eq!(p.output_height, 1440);
        assert_eq!(width_for(3840, 2160, 1440), 2560);
        assert_eq!(recommend(&hw).preset, Preset::Balanced);
    }
}
