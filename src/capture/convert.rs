//! GPU color conversion and scaling: captured BGRA -> encoder-sized NV12.
//!
//! The D3D11 *video processor* is fixed-function hardware (the same block
//! video players use for scaling), so conversion costs almost no shader time
//! and doesn't compete with the game's rendering much.
//!
//! Readback is pipelined by one frame: `submit()` queues GPU work and returns
//! immediately; the *next* tick's `read_pending()` maps the result, which the
//! GPU finished long ago. Mapping in the same tick would stall the CPU until
//! the GPU caught up, which under a heavy game can take several milliseconds.

use std::mem::ManuallyDrop;

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{BOOL, Interface};

use super::d3d::D3D;
use super::screenshot::BgraImage;

pub struct Nv12Converter {
    d3d: D3D,
    video_context: ID3D11VideoContext1,
    processor: ID3D11VideoProcessor,
    // Kept alive because views/processor were created from it.
    _enumerator: ID3D11VideoProcessorEnumerator,
    pub in_w: u32,
    pub in_h: u32,
    pub out_w: u32,
    pub out_h: u32,
    input_tex: ID3D11Texture2D,
    input_view: ID3D11VideoProcessorInputView,
    output_tex: ID3D11Texture2D,
    output_view: ID3D11VideoProcessorOutputView,
    staging: [ID3D11Texture2D; 2],
    next_slot: usize,
    pending: Option<usize>,
    has_input: bool,
}

// SAFETY: all D3D11 objects here belong to a device with multithread
// protection enabled; we only ever use the converter from one thread at a time.
unsafe impl Send for Nv12Converter {}

fn texture(
    d3d: &D3D,
    w: u32,
    h: u32,
    format: DXGI_FORMAT,
    usage: D3D11_USAGE,
    bind: u32,
    cpu: u32,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: usage,
        BindFlags: bind,
        CPUAccessFlags: cpu,
        MiscFlags: 0,
    };
    let mut tex = None;
    // SAFETY: desc is fully initialized; out-param is a valid Option.
    unsafe { d3d.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
    tex.context("CreateTexture2D returned null")
}

/// Where the captured picture goes in the output: all of it. Every source
/// (a monitor or one app's window) is stretched to fill the clip frame, so a
/// game at a custom resolution (e.g. 4:3 "stretched res") looks the way the
/// player sees it, with no black bars. The output size is even (NV12), so
/// the edges stay chroma-aligned.
pub fn fill_rect(out_w: u32, out_h: u32) -> RECT {
    RECT { left: 0, top: 0, right: out_w as i32, bottom: out_h as i32 }
}

impl Nv12Converter {
    pub fn new(d3d: &D3D, in_w: u32, in_h: u32, out_w: u32, out_h: u32) -> Result<Self> {
        if !out_w.is_multiple_of(2) || !out_h.is_multiple_of(2) {
            bail!("NV12 output size must be even, got {out_w}x{out_h}");
        }
        let video_device: ID3D11VideoDevice = d3d.device.cast().context("GPU has no D3D11 video device")?;
        let video_context: ID3D11VideoContext1 = d3d
            .context
            .cast()
            .context("ID3D11VideoContext1 unavailable (needs Windows 8.1+)")?;

        let rate = DXGI_RATIONAL { Numerator: 60, Denominator: 1 };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: in_w,
            InputHeight: in_h,
            OutputFrameRate: rate,
            OutputWidth: out_w,
            OutputHeight: out_h,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        // SAFETY: content desc is fully initialized.
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&content)? };
        // SAFETY: valid enumerator.
        let support = unsafe { enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_NV12)? };
        if support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0 {
            bail!("GPU video processor cannot output NV12");
        }
        // SAFETY: valid enumerator; rate conversion index 0 always exists.
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0)? };

        let rt = D3D11_BIND_RENDER_TARGET.0 as u32;
        let srv = D3D11_BIND_SHADER_RESOURCE.0 as u32;
        let input_tex = texture(d3d, in_w, in_h, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_USAGE_DEFAULT, rt | srv, 0)?;
        let output_tex = texture(d3d, out_w, out_h, DXGI_FORMAT_NV12, D3D11_USAGE_DEFAULT, rt, 0)?;
        let read = D3D11_CPU_ACCESS_READ.0 as u32;
        let staging = [
            texture(d3d, out_w, out_h, DXGI_FORMAT_NV12, D3D11_USAGE_STAGING, 0, read)?,
            texture(d3d, out_w, out_h, DXGI_FORMAT_NV12, D3D11_USAGE_STAGING, 0, read)?,
        ];

        let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
            },
        };
        let mut input_view = None;
        // SAFETY: texture and enumerator are valid; desc initialized.
        unsafe { video_device.CreateVideoProcessorInputView(&input_tex, &enumerator, &in_desc, Some(&mut input_view))? };
        let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut output_view = None;
        // SAFETY: as above.
        unsafe { video_device.CreateVideoProcessorOutputView(&output_tex, &enumerator, &out_desc, Some(&mut output_view))? };

        // SAFETY: plain state setters on valid objects.
        unsafe {
            let vc = &video_context;
            vc.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            // Disable driver "enhancements" (sharpening, color tweaks) so the
            // clip looks exactly like the screen.
            vc.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            // Desktop = full-range sRGB in; BT.709 limited-range YUV out. The
            // encoder args tag the stream the same way so players match.
            vc.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
            vc.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            let src = RECT { left: 0, top: 0, right: in_w as i32, bottom: in_h as i32 };
            vc.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src));
            let dst = fill_rect(out_w, out_h);
            vc.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dst));
            vc.VideoProcessorSetOutputTargetRect(&processor, true, Some(&dst));
            // Nothing should show around the picture any more; if a driver
            // ever leaves an edge pixel uncovered, it's black.
            let black = D3D11_VIDEO_COLOR {
                Anonymous: D3D11_VIDEO_COLOR_0 {
                    RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 },
                },
            };
            vc.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);
        }

        log::debug!("NV12 converter {in_w}x{in_h} -> {out_w}x{out_h}");
        Ok(Self {
            d3d: d3d.clone(),
            video_context,
            processor,
            _enumerator: enumerator,
            in_w,
            in_h,
            out_w,
            out_h,
            input_tex,
            input_view: input_view.context("null input view")?,
            output_tex,
            output_view: output_view.context("null output view")?,
            staging,
            next_slot: 0,
            pending: None,
            has_input: false,
        })
    }

    /// Queues: copy `src` (a captured frame, cropped to `w`x`h`) into our input
    /// texture, convert/scale it to NV12, and copy that into a staging slot.
    /// Doesn't wait for the GPU.
    pub fn submit(&mut self, src: &ID3D11Texture2D, w: u32, h: u32) -> Result<()> {
        let ctx = &self.d3d.context;
        let bx = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: w.min(self.in_w),
            bottom: h.min(self.in_h),
            back: 1,
        };
        // SAFETY: both textures are valid BGRA textures on the same device;
        // the box is clamped to the destination size.
        unsafe { ctx.CopySubresourceRegion(&self.input_tex, 0, 0, 0, 0, src, 0, Some(&bx)) };
        self.has_input = true;

        // The stream struct holds a COM pointer in a ManuallyDrop (it's a C
        // struct); we give it a clone and drop that clone ourselves after.
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: BOOL(1),
            pInputSurface: ManuallyDrop::new(Some(self.input_view.clone())),
            ..Default::default()
        };
        // SAFETY: processor/views are valid; one stream.
        let result = unsafe {
            self.video_context
                .VideoProcessorBlt(&self.processor, &self.output_view, 0, std::slice::from_ref(&stream))
        };
        // SAFETY: we put exactly one owned reference in there above.
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        result.context("VideoProcessorBlt failed")?;

        let slot = self.next_slot;
        self.next_slot ^= 1;
        // SAFETY: same size/format textures on the same device.
        unsafe {
            ctx.CopyResource(&self.staging[slot], &self.output_tex);
            // Kick the queued commands to the GPU now; otherwise D3D would hold
            // them until the Map() next tick and we'd stall there.
            ctx.Flush();
        }
        self.pending = Some(slot);
        Ok(())
    }

    /// If a frame was submitted, copies it into `out` as tightly packed NV12
    /// (`w*h` luma bytes, then `w*h/2` interleaved chroma bytes). Returns
    /// whether `out` was written.
    pub fn read_pending(&mut self, out: &mut [u8]) -> Result<bool> {
        let Some(slot) = self.pending.take() else {
            return Ok(false);
        };
        let (w, h) = (self.out_w as usize, self.out_h as usize);
        if out.len() < w * h * 3 / 2 {
            bail!("NV12 buffer too small");
        }
        let ctx = &self.d3d.context;
        let tex = &self.staging[slot];
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: staging texture with CPU read access.
        unsafe { ctx.Map(tex, 0, D3D11_MAP_READ, 0, Some(&mut mapped))? };
        let pitch = mapped.RowPitch as usize;
        let base = mapped.pData as *const u8;
        // SAFETY: a mapped NV12 texture is `pitch * h` bytes of Y followed by
        // `pitch * h/2` bytes of interleaved UV; we read `w` bytes per row.
        unsafe {
            for row in 0..h {
                let src = std::slice::from_raw_parts(base.add(row * pitch), w);
                out[row * w..row * w + w].copy_from_slice(src);
            }
            let uv_base = base.add(pitch * h);
            let uv_out = &mut out[w * h..];
            for row in 0..h / 2 {
                let src = std::slice::from_raw_parts(uv_base.add(row * pitch), w);
                uv_out[row * w..row * w + w].copy_from_slice(src);
            }
            ctx.Unmap(tex, 0);
        }
        Ok(true)
    }

    /// Full-resolution BGRA copy of the most recently submitted frame. Used for
    /// screenshots, so it blocks briefly on the GPU; that's fine off the hot path.
    pub fn read_input_bgra(&self) -> Result<BgraImage> {
        if !self.has_input {
            bail!("no frame captured yet");
        }
        let read = D3D11_CPU_ACCESS_READ.0 as u32;
        let staging = texture(&self.d3d, self.in_w, self.in_h, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_USAGE_STAGING, 0, read)?;
        let ctx = &self.d3d.context;
        let (w, h) = (self.in_w as usize, self.in_h as usize);
        let mut data = vec![0u8; w * h * 4];
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: same-size BGRA textures; staging has CPU read access.
        unsafe {
            ctx.CopyResource(&staging, &self.input_tex);
            ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let pitch = mapped.RowPitch as usize;
            let base = mapped.pData as *const u8;
            for row in 0..h {
                let src = std::slice::from_raw_parts(base.add(row * pitch), w * 4);
                data[row * w * 4..(row + 1) * w * 4].copy_from_slice(src);
            }
            ctx.Unmap(&staging, 0);
        }
        Ok(BgraImage {
            width: self.in_w,
            height: self.in_h,
            data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::fill_rect;

    #[test]
    fn every_source_fills_the_whole_clip_frame() {
        // Whatever the captured size (4:3 stretched res, a tall window, a
        // 5:4 monitor), the picture covers the full output: no bars.
        for (out_w, out_h) in [(1920, 1080), (1280, 720), (2560, 1440), (854, 480)] {
            let r = fill_rect(out_w, out_h);
            assert_eq!((r.left, r.top, r.right, r.bottom), (0, 0, out_w as i32, out_h as i32));
        }
    }
}
