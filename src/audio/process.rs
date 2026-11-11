//! Per-app audio: WASAPI "process loopback" (Windows 10 2004 and later).
//!
//! Windows can capture the sound of one process (and the processes it
//! started) no matter which playback device it plays on. That's what makes
//! "Discord on the Chat device + the game on the Game device" work without
//! capturing everything else on those devices.
//!
//! The capture client comes from `ActivateAudioInterfaceAsync`, which
//! reports back through a COM callback; we wait for it on the audio thread.

use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
    IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX,
};
use windows::Win32::System::Com::BLOB;
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
use windows::Win32::System::Threading::CreateEventW;
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{HRESULT, IUnknown, Interface};
use windows_core::implement;

use super::{AudioPacket, AudioSource, CHANNELS, SAMPLE_RATE, qpc_now_100ns};

/// WAVE_FORMAT_IEEE_FLOAT (from mmreg.h).
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

/// Signals the waiting thread when Windows finishes activating.
#[implement(IActivateAudioInterfaceCompletionHandler, windows_core::imp::IAgileObject)]
struct Done(mpsc::SyncSender<()>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Done_Impl {
    fn ActivateCompleted(&self, _op: windows::core::Ref<IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        let _ = self.0.try_send(());
        Ok(())
    }
}

impl windows_core::imp::IAgileObject_Impl for Done_Impl {}

pub struct ProcessLoopbackSource {
    pub pid: u32,
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: HANDLE,
    /// Where the next packet is expected on the QPC clock (used when a
    /// packet carries no timestamp).
    next_qpc: Option<u64>,
    failed: bool,
}

// SAFETY: created and used only on the recorder's audio thread.
unsafe impl Send for ProcessLoopbackSource {}

impl ProcessLoopbackSource {
    /// Starts capturing everything process `pid` (and its child processes)
    /// plays, on any device.
    pub fn new(pid: u32) -> Result<ProcessLoopbackSource> {
        let params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        };
        // The activation parameters travel as a PROPVARIANT holding a blob.
        // ManuallyDrop: dropping a PROPVARIANT frees its blob with
        // CoTaskMemFree, but this blob points at `params` on our stack.
        let prop = std::mem::ManuallyDrop::new(PROPVARIANT {
            Anonymous: PROPVARIANT_0 {
                Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                    vt: VT_BLOB,
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: PROPVARIANT_0_0_0 {
                        blob: BLOB {
                            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                            pBlobData: &params as *const _ as *mut u8,
                        },
                    },
                }),
            },
        });
        let (tx, rx) = mpsc::sync_channel(1);
        let handler: IActivateAudioInterfaceCompletionHandler = Done(tx).into();
        // SAFETY: `params`/`prop` outlive the call (Windows copies the blob
        // during it); the handler is a valid COM object.
        let op = unsafe {
            ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*prop), &handler)
        }
        .context("per-app audio isn't supported on this Windows version")?;
        rx.recv_timeout(Duration::from_secs(5)).map_err(|_| anyhow!("per-app audio activation timed out"))?;
        let mut hr = HRESULT(0);
        let mut unknown: Option<IUnknown> = None;
        // SAFETY: activation completed; out-params are valid.
        unsafe { op.GetActivateResult(&mut hr, &mut unknown)? };
        hr.ok().context("per-app audio activation failed")?;
        let client: IAudioClient = unknown.context("no audio client")?.cast()?;

        let block = (CHANNELS * 4) as u16;
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            nChannels: CHANNELS as u16,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * block as u32,
            nBlockAlign: block,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        // SAFETY: standard WASAPI setup on the activated client. Process
        // loopback requires event-driven mode, so an event is set even though
        // we poll.
        unsafe {
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                    2_000_000, // 200 ms
                    0,
                    &format,
                    None,
                )
                .context("starting per-app capture")?;
            let event = CreateEventW(None, false, false, None)?;
            client.SetEventHandle(event)?;
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            Ok(ProcessLoopbackSource { pid, client, capture, event, next_qpc: None, failed: false })
        }
    }

    pub fn failed(&self) -> bool {
        self.failed
    }

    fn drain(&mut self, out: &mut Vec<AudioPacket>) -> Result<()> {
        // SAFETY: GetBuffer/ReleaseBuffer pairs on a started capture client;
        // data is copied before release.
        unsafe {
            while self.capture.GetNextPacketSize()? > 0 {
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                let mut qpc = 0u64;
                self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
                let n = frames as usize * CHANNELS;
                let samples = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    vec![0.0f32; n]
                } else {
                    std::slice::from_raw_parts(data as *const f32, n).to_vec()
                };
                self.capture.ReleaseBuffer(frames)?;
                let dur = frames as u64 * 10_000_000 / SAMPLE_RATE as u64;
                // No timestamp from Windows: continue from the previous packet,
                // or assume it just finished playing.
                let qpc = if qpc != 0 {
                    qpc
                } else {
                    self.next_qpc.unwrap_or_else(|| qpc_now_100ns().saturating_sub(dur))
                };
                self.next_qpc = Some(qpc + dur);
                out.push(AudioPacket { qpc_100ns: qpc, samples, stream: 0 });
            }
        }
        Ok(())
    }
}

impl AudioSource for ProcessLoopbackSource {
    fn poll(&mut self, out: &mut Vec<AudioPacket>, _start: u64, _now: u64) {
        if self.failed {
            return;
        }
        if let Err(e) = self.drain(out) {
            log::info!("per-app audio for process {} ended ({e:#})", self.pid);
            self.failed = true;
        }
    }
}

impl Drop for ProcessLoopbackSource {
    fn drop(&mut self) {
        // SAFETY: valid client and event handle.
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}
