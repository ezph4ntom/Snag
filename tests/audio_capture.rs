//! PC audio from several apps on several playback devices.
//!
//! Two helper processes (this test binary, restarted in "tone" mode) play
//! quiet tones on two different playback devices: 440 Hz on one, 1000 Hz on
//! the other. Then:
//! * per-app capture of the first helper hears its tone and not the other;
//! * a real recording in "Whole PC audio" mode contains both tones;
//! * a recording with only the helper app ticked contains both (same exe).
//!
//! Needs two playback devices besides the default one (it avoids the default
//! device, usually the headset you wear) and ffmpeg.
//!
//! cargo test --release --test audio_capture -- --ignored --nocapture

use std::process::{Child, Command};
use std::time::{Duration, Instant};

use snag::audio::devices::{self, RenderDevice};
use snag::audio::mixer::{MixSource, SharedSelection};
use snag::audio::process::ProcessLoopbackSource;
use snag::audio::{AudioPacket, AudioSource, AudioSourceFactory, qpc_now_100ns};
use snag::buffer::clip::save_clip;
use snag::buffer::source::{FrameSource, SyntheticSource};
use snag::buffer::{Recorder, RecorderConfig, SourceFactory};
use snag::config::{AudioAppChoice, AudioSettings};

const AMPLITUDE: f32 = 0.03; // about -30 dB: quiet, but far above silence

/// Plays a tone on one device for a while (runs in the helper process).
fn play_tone(device_id: &str, hz: f32, seconds: f32) {
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::*;
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let e: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap();
        let wide: Vec<u16> = device_id.encode_utf16().chain([0]).collect();
        let dev = e.GetDevice(windows::core::PCWSTR(wide.as_ptr())).unwrap();
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).unwrap();
        let format = WAVEFORMATEX {
            wFormatTag: 3,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 48_000 * 8,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                1_000_000,
                0,
                &format,
                None,
            )
            .unwrap();
        let render: IAudioRenderClient = client.GetService().unwrap();
        let size = client.GetBufferSize().unwrap() as usize;
        client.Start().unwrap();
        let t0 = Instant::now();
        let mut n: u64 = 0;
        while t0.elapsed().as_secs_f32() < seconds {
            let free = size - client.GetCurrentPadding().unwrap() as usize;
            if free > 0 {
                let buf = render.GetBuffer(free as u32).unwrap();
                let out = std::slice::from_raw_parts_mut(buf as *mut f32, free * 2);
                for f in 0..free {
                    let v = AMPLITUDE * (2.0 * std::f32::consts::PI * hz * n as f32 / 48_000.0).sin();
                    out[2 * f] = v;
                    out[2 * f + 1] = v;
                    n += 1;
                }
                render.ReleaseBuffer(free as u32, 0).unwrap();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = client.Stop();
    }
}

/// The helper: plays a tone when started with SNAG_TONE_DEVICE set.
#[test]
#[ignore = "helper for captures_apps_on_different_devices"]
fn tone_helper() {
    if let Ok(dev) = std::env::var("SNAG_TONE_DEVICE") {
        let hz: f32 = std::env::var("SNAG_TONE_HZ").unwrap().parse().unwrap();
        play_tone(&dev, hz, 40.0);
    }
}

fn spawn_tone(device: &RenderDevice, hz: u32) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "tone_helper", "--nocapture", "--test-threads=1"])
        .env("SNAG_TONE_DEVICE", &device.id)
        .env("SNAG_TONE_HZ", hz.to_string())
        .spawn()
        .unwrap()
}

/// Power of one frequency in a mono signal (Goertzel), normalized so a
/// full-length sine of amplitude A gives about A.
fn tone_level(mono: &[f32], hz: f32) -> f32 {
    let w = 2.0 * std::f32::consts::PI * hz / 48_000.0;
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for &x in mono {
        let s = x + 2.0 * w.cos() * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    let power = s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2;
    2.0 * power.max(0.0).sqrt() / mono.len().max(1) as f32
}

fn mono(packets: &[AudioPacket]) -> Vec<f32> {
    packets.iter().flat_map(|p| p.samples.as_chunks::<2>().0.iter().map(|c| (c[0] + c[1]) / 2.0)).collect()
}

/// Records ~3 s with `selection` and returns the clip's audio (mono).
fn record(selection: AudioSettings, name: &str) -> Vec<f32> {
    let probes = snag::encode::detect::probe_all();
    let enc = probes.iter().find(|p| p.works).expect("an encoder");
    let root = std::env::temp_dir().join(format!("snag-audio-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (w, h) = (640, 360);
    let source: SourceFactory = Box::new(move || Ok(Box::new(SyntheticSource::new(w, h)) as Box<dyn FrameSource>));
    let shared = SharedSelection::new(selection);
    let audio: AudioSourceFactory = Box::new(move || Box::new(MixSource::new(shared)) as Box<dyn AudioSource>);
    let rec = Recorder::start(
        RecorderConfig {
            encoder: enc.kind,
            width: w,
            height: h,
            fps: 30,
            bitrate_kbps: 2_000,
            buffer_seconds: 3,
            segment_seconds: 1,
            session_dir: root.join("session"),
            gpu_upload: enc.gpu_upload,
            audio_bitrate_kbps: 160,
        },
        source,
        Some(audio),
    )
    .unwrap();
    let t0 = Instant::now();
    while rec.session.buffered_seconds(3) < 3.0 {
        assert!(t0.elapsed() < Duration::from_secs(20), "recorder didn't start: {:?}", rec.session.error());
        std::thread::sleep(Duration::from_millis(50));
    }
    let sources = snag::audio::mixer::capture_state();
    println!("{name}: recording from {:?} {:?}", sources.sources, sources.note);
    let clip = save_clip(&rec.session, Instant::now(), 3.0, &root).unwrap().path;
    rec.stop();
    let out = Command::new(snag::paths::ffmpeg_exe())
        .args(["-v", "error", "-i"])
        .arg(&clip)
        .args(["-map", "0:a:0", "-f", "f32le", "-ac", "1", "-ar", "48000", "pipe:1"])
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&root);
    out.stdout.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect()
}

#[test]
#[ignore = "needs two spare playback devices and ffmpeg; plays quiet tones"]
fn captures_apps_on_different_devices() {
    snag::logging::init("test-audio");
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
    }
    let all = devices::render_devices().unwrap();
    for d in &all {
        println!("device: {}{}", d.name, if d.is_default { " (default)" } else { "" });
    }
    // Spare devices: not the default one, and not a headset's chat side
    // (both are probably on someone's ears).
    let mut spare: Vec<&RenderDevice> =
        all.iter().filter(|d| !d.is_default && !d.name.to_lowercase().contains("headset")).collect();
    spare.sort_by_key(|d| !d.name.contains("S/PDIF"));
    assert!(spare.len() >= 2, "needs two spare playback devices, found {}", spare.len());
    let (dev_a, dev_b) = (spare[0], spare[1]);
    println!("tone A 440 Hz on {}, tone B 1000 Hz on {}", dev_a.name, dev_b.name);

    let mut a = spawn_tone(dev_a, 440);
    let mut b = spawn_tone(dev_b, 1000);
    std::thread::sleep(Duration::from_millis(1500));

    // Both helpers show up as audio sessions on their own devices.
    let sessions = devices::sessions();
    assert!(sessions.iter().any(|s| s.pid == a.id() && s.device_id == dev_a.id), "helper A's session on device A");
    assert!(sessions.iter().any(|s| s.pid == b.id() && s.device_id == dev_b.id), "helper B's session on device B");

    // Per-app capture of A: its tone, not B's.
    let mut cap = ProcessLoopbackSource::new(a.id()).expect("per-app capture");
    let mut packets = Vec::new();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(1) {
        let now = qpc_now_100ns();
        cap.poll(&mut packets, now, now);
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(cap);
    let m = mono(&packets);
    let (l440, l1000) = (tone_level(&m, 440.0), tone_level(&m, 1000.0));
    println!("per-app capture of A: 440 Hz {l440:.4}, 1000 Hz {l1000:.4} ({} samples)", m.len());
    assert!(l440 > AMPLITUDE * 0.3, "A's tone is captured");
    assert!(l1000 < AMPLITUDE * 0.05, "B's tone (other app, other device) is not");

    // A recording of the whole PC has both.
    let whole = record(AudioSettings { whole_pc: true, record_mic: false, ..Default::default() }, "whole");
    let (l440, l1000) = (tone_level(&whole, 440.0), tone_level(&whole, 1000.0));
    println!("whole-PC clip: 440 Hz {l440:.4}, 1000 Hz {l1000:.4} ({:.2} s)", whole.len() as f64 / 48_000.0);
    assert!(l440 > AMPLITUDE * 0.3 && l1000 > AMPLITUDE * 0.3, "both devices are in the clip");

    // Only the helper app ticked (both helpers are the same exe).
    let exe = devices::exe_key(&std::env::current_exe().unwrap());
    let apps = AudioSettings {
        whole_pc: false,
        record_mic: false,
        apps: vec![AudioAppChoice { exe, name: "helper".into(), include: true, volume: 1.0 }],
        ..Default::default()
    };
    let only = record(apps, "apps");
    let (l440, l1000) = (tone_level(&only, 440.0), tone_level(&only, 1000.0));
    println!("ticked-apps clip: 440 Hz {l440:.4}, 1000 Hz {l1000:.4}");
    assert!(l440 > AMPLITUDE * 0.3 && l1000 > AMPLITUDE * 0.3, "both helpers are in the clip");

    // Nothing ticked: silent clip.
    let none = record(AudioSettings { whole_pc: false, record_mic: false, ..Default::default() }, "none");
    let peak = none.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    println!("nothing-ticked clip: peak {peak:.5}");
    assert!(peak < 1e-3, "nothing ticked records silence");

    // The microphone: captured and mixed in (whatever it hears; at least
    // the source opens and delivers audio).
    let mut mic = snag::audio::loopback::LoopbackSource::microphone(None);
    let mut packets = Vec::new();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(1) {
        let now = qpc_now_100ns();
        mic.poll(&mut packets, now, now);
        std::thread::sleep(Duration::from_millis(10));
    }
    let frames: usize = packets.iter().map(|p| p.samples.len() / 2).sum();
    println!("microphone: {frames} frames in 1 s from {:?}", devices::capture_devices().unwrap().iter().find(|d| d.is_default).map(|d| d.name.clone()));
    assert!(frames > 40_000, "the default microphone delivers audio");

    let _ = a.kill();
    let _ = a.wait();
    let _ = b.kill();
    let _ = b.wait();
}

/// Prints how late each kind of source delivers its packets (arrival time
/// minus the packet's own timestamp). The recorder waits 50 ms plus the
/// capture latency for stragglers.
#[test]
#[ignore = "diagnostic; needs audio devices"]
fn source_latency() {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
    }
    let mut sources: Vec<(String, Box<dyn AudioSource>)> = vec![
        ("default output".into(), Box::new(snag::audio::loopback::LoopbackSource::new())),
        ("microphone".into(), Box::new(snag::audio::loopback::LoopbackSource::microphone(None))),
    ];
    for d in devices::render_devices().unwrap() {
        sources.push((d.name.clone(), Box::new(snag::audio::loopback::LoopbackSource::for_device(&d.id))));
    }
    let mut stats: Vec<(usize, f64, f64)> = vec![(0, 0.0, 0.0); sources.len()];
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(3) {
        for (i, (_, s)) in sources.iter_mut().enumerate() {
            let mut p = Vec::new();
            let now = qpc_now_100ns();
            s.poll(&mut p, now, now);
            for pk in p {
                let frames = pk.samples.len() / 2;
                let end = pk.qpc_100ns + frames as u64 * 10_000_000 / 48_000;
                let late_ms = (now as f64 - end as f64) / 10_000.0;
                stats[i].0 += 1;
                stats[i].1 += late_ms;
                stats[i].2 = stats[i].2.max(late_ms);
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    for ((name, _), (n, sum, max)) in sources.iter().zip(stats) {
        println!("{name}: {n} packets, avg {:.1} ms late, max {max:.1} ms", if n > 0 { sum / n as f64 } else { 0.0 });
    }
}
