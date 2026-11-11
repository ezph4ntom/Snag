//! Direct3D 11 device setup.

use anyhow::{Context, Result};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;
use windows::core::Interface;

/// The D3D11 device in its three "shapes":
/// * `device`/`context`: classic Win32 COM interfaces for our own GPU work.
/// * `winrt_device`: the same device wrapped as a WinRT object, which is what
///   the Windows Graphics Capture API wants.
#[derive(Clone)]
pub struct D3D {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub winrt_device: IDirect3DDevice,
}

// SAFETY: D3D11 devices are free-threaded. The immediate context is not, but
// we turn on multithread protection below so D3D serializes calls on it.
unsafe impl Send for D3D {}

impl D3D {
    pub fn new() -> Result<Self> {
        let mut device = None;
        let mut context = None;
        // SAFETY: standard device creation; out-params are valid Options.
        unsafe {
            D3D11CreateDevice(
                None, // default adapter: the one driving the primary display
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                // BGRA: capture frames are BGRA. VIDEO: we use the video
                // processor for color conversion/scaling.
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("D3D11CreateDevice failed (is a GPU driver installed?)")?;
        }
        let device: ID3D11Device = device.context("no D3D11 device")?;
        let context: ID3D11DeviceContext = context.context("no D3D11 context")?;

        // Screenshots may touch the context from another thread than the
        // pacer; let D3D lock it internally.
        if let Ok(mt) = context.cast::<ID3D11Multithread>() {
            // SAFETY: simple setter on a valid interface.
            unsafe {
                let _ = mt.SetMultithreadProtected(true);
            }
        }

        let dxgi: IDXGIDevice = device.cast()?;
        // Ask the GPU scheduler to favor our (tiny) workload so a game that
        // saturates the GPU can't starve capture into dropped frames. Without
        // admin rights Windows may refuse; that's fine.
        // SAFETY: valid interface; range is -7..=7.
        unsafe {
            let _ = dxgi.SetGPUThreadPriority(7);
        }
        // SAFETY: wraps the DXGI device as a WinRT IDirect3DDevice.
        let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? };
        let winrt_device: IDirect3DDevice = inspectable.cast()?;

        Ok(Self {
            device,
            context,
            winrt_device,
        })
    }
}
