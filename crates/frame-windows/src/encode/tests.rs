//! Deterministic encoder seam tests (FRAME-0104).
//!
//! These tests drive the production worker, facade and accounting through the private
//! [`super::driver::NvencDriver`] seam with a scripted driver. They need no GPU, no
//! driver and no capture session, and they are not hardware evidence: they establish the
//! code paths and the accounting, not native behaviour. The native hardware evidence
//! lives in `tests/hardware_encode.rs` and in the recorded proof runs.

use super::held_credits_available;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::num::{NonZeroU32, NonZeroU64};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use frame_capture::CapturedFrame;
use frame_clock::MediaTimestamp;
use frame_contracts::{ErrorCode, RecorderError};
use frame_encode::{EncoderInput, EncoderPoll, PacketEncoder, SubmitOutcome};
use frame_mux::ContainerWriter;

use super::driver::{
    EncodeTexture, NativeCollect, NativeEos, NativePacket, NativeStats, NativeSubmit, NvencDriver,
    SurfaceIdentity,
};
use super::{
    MAX_ACCESS_UNIT_BYTES, MAX_CODEC_CONFIG_BYTES, MAX_ENCODER_LEASES, MAX_PACKET_BYTES,
    MAX_PACKETS, NativeEncoderConfig, NativeEncoderPreset, NativeEncoderSettings,
    NativeRateControl, WindowsH264Encoder, internal_error,
};
use crate::surface::{
    PayloadLifetime, PayloadOwner, SLOT_COUNT, SurfaceFormat, SurfacePool, SurfaceSignal,
    WindowsSurface, plan_owned_payload,
};

/// A real NVIDIA encode/mux fixture. The source is a known FP16 scRGB texture,
/// so this does not sample the user's desktop. Run explicitly with --ignored.
#[test]
#[ignore = "requires a local NVENC Main10 GPU; writes a synthetic MKV into the temp directory"]
fn native_synthetic_hdr_main10_encode_and_mux() {
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC,
    };
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
    use windows::core::Interface;

    struct SyntheticHdr {
        device: ID3D11Device,
        textures: Vec<ID3D11Texture2D>,
        pool: u64,
        width: u32,
        height: u32,
    }
    impl PayloadOwner for SyntheticHdr {
        fn encode_texture(&self, index: usize) -> Option<EncodeTexture> {
            let texture = self.textures.get(index)?;
            Some(EncodeTexture {
                device: self.device.as_raw(),
                texture: texture.as_raw(),
                identity: SurfaceIdentity {
                    pool: self.pool,
                    device: self.device.as_raw() as usize,
                    texture: texture.as_raw() as usize,
                    width: self.width,
                    height: self.height,
                },
            })
        }
        fn format(&self) -> SurfaceFormat {
            SurfaceFormat::ScRgbFloat16
        }
        fn signal(&self) -> Option<SurfaceSignal> {
            Some(SurfaceSignal::SCRGB)
        }
    }

    let qualified = super::probe_native_hevc_main10().expect("Main10 qualification probe");
    let Some(candidate) = qualified.first() else {
        eprintln!("SKIP: this machine has no qualified NVIDIA HEVC Main10 adapter");
        return;
    };
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.expect("DXGI factory");
    let mut device = None;
    for index in 0..super::MAX_SCANNED_ENCODE_ADAPTERS as u32 {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(index) }) else {
            break;
        };
        let desc = unsafe { adapter.GetDesc1() }.expect("DXGI adapter description");
        let luid =
            u64::from(desc.AdapterLuid.LowPart) | ((desc.AdapterLuid.HighPart as u32 as u64) << 32);
        if luid == candidate.adapter.luid {
            device = Some(
                super::native::qualification_device(&factory, index).expect("D3D11 NVIDIA device"),
            );
            break;
        }
    }
    let device = device.expect("qualified adapter must still be attached");
    let context = unsafe { device.GetImmediateContext() }.expect("D3D11 context");
    let multithread: ID3D11Multithread = context.cast().expect("D3D11 multithread interface");
    let _ = unsafe { multithread.SetMultithreadProtected(true) };
    let width = 256u32;
    let height = 256u32;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .expect("synthetic FP16 texture");
    let texture = texture.expect("D3D11 returned FP16 texture");
    // Half floats: 1.0, 0.5, 0.25, 4.0. The right half is HDR bright.
    let mut pixels = Vec::<u16>::with_capacity((width * height * 4) as usize);
    for _y in 0..height {
        for x in 0..width {
            pixels.extend_from_slice(if x < width / 2 {
                &[0x3c00, 0x3800, 0x3400, 0x3c00]
            } else {
                &[0x4400, 0x3c00, 0x3800, 0x3c00]
            });
        }
    }
    unsafe { context.UpdateSubresource(&texture, 0, None, pixels.as_ptr().cast(), width * 8, 0) };
    let mut textures = vec![texture];
    for _ in 1..SLOT_COUNT {
        let mut texture = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }.unwrap();
        let texture = texture.unwrap();
        unsafe {
            context.UpdateSubresource(&texture, 0, None, pixels.as_ptr().cast(), width * 8, 0)
        };
        textures.push(texture);
    }
    let pool = SurfacePool::new(u64::from(width) * u64::from(height) * 8 * 4);
    let payload: Arc<dyn PayloadOwner> = Arc::new(SyntheticHdr {
        device,
        textures,
        pool: pool.id(),
        width,
        height,
    });
    let slot = pool.try_acquire_copy_slot().expect("synthetic slot");
    slot.mark_ready();
    slot.mark_consumer_owned();
    let preparation = WindowsSurface::preparation(slot, Arc::clone(&payload));
    let config = NativeEncoderConfig::with_quality(
        width,
        height,
        NonZeroU32::new(60).unwrap(),
        NonZeroU32::new(1).unwrap(),
        super::NativeQualitySettings {
            bitrate_bps: 4_000_000,
            peak_bitrate_bps: 4_000_000,
            gop_frames: 30,
            rate_control: NativeRateControl::Cbr,
            preset: NativeEncoderPreset::LowLatency,
        },
    )
    .expect("Main10 fixture configuration");
    let mut encoder = WindowsH264Encoder::open_hevc_main10(config, &preparation)
        .expect("native HEVC Main10 session");
    drop(preparation);
    let slot = pool.try_acquire_copy_slot().unwrap();
    slot.mark_ready();
    slot.mark_consumer_owned();
    let surface = WindowsSurface::new(slot, payload);
    let stream = encoder.info().clone();
    assert_eq!(
        stream.bitstream_format,
        frame_encode::BitstreamFormat::HevcAnnexB
    );
    let frame = CapturedFrame {
        surface,
        timestamp: MediaTimestamp(0),
        width,
        height,
    };
    assert!(matches!(
        encoder
            .submit(EncoderInput {
                input: frame,
                duration_ns: NonZeroU64::new(16_666_667).unwrap(),
            })
            .expect("submit synthetic HDR frame"),
        SubmitOutcome::Accepted
    ));
    encoder.begin_drain().expect("submit HEVC EOS");
    let packets = collect_packets(&mut encoder, 1, Duration::from_secs(5));
    assert_eq!(packets.len(), 1, "native HEVC packet must arrive");
    wait_drained(&mut encoder, Duration::from_secs(5)).expect("native HEVC drain");
    encoder.stop().expect("native HEVC close");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let output = std::env::temp_dir().join(format!("frame0403-native-main10-{nonce}.mkv"));
    let mut writer = frame_mux::MkvVideoWriter::create(&output, &stream, MediaTimestamp(0))
        .expect("HEVC Matroska create");
    for packet in packets {
        writer.write(packet).expect("HEVC Matroska block");
    }
    writer.finalize().expect("HEVC Matroska finalize");
    eprintln!("NATIVE_MAIN10_MKV={}", output.display());
}

#[test]
fn native_input_profiles_preserve_signal_and_reject_incompatible_depth() {
    use super::{NativeVideoProfile as P, native_input_signal};
    use crate::surface::{SurfaceFormat as F, SurfaceSignal};
    let mut colour = frame_encode::VideoColour::BT2020_PQ_LIMITED;
    colour.transfer_characteristics = 18;
    colour.range = 2;
    let signal = Some(SurfaceSignal {
        colour,
        reference_white_nits: 0,
    });
    assert_eq!(
        native_input_signal(P::Av1Main10, F::P010, signal).unwrap(),
        (2, colour, false)
    );
    assert_eq!(
        native_input_signal(P::HevcMain10, F::P010, signal).unwrap(),
        (2, colour, false)
    );
    assert!(native_input_signal(P::H264High, F::P010, signal).is_err());
    assert!(native_input_signal(P::Av1Main8, F::P010, signal).is_err());
    assert!(native_input_signal(P::Av1Main10, F::P010, None).is_err());
    assert!(native_input_signal(P::HevcMain10, F::Bgra8, None).is_err());
}

#[test]
#[ignore = "local NVENC GPU smoke; supply FREYM_CODEC_SMOKE_DIR"]
fn native_yuv_profiles_mux_smoke() {
    use super::NativeVideoProfile as P;
    use crate::surface::{SurfaceFormat, SurfaceSignal, SurfaceSlot};
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::core::Interface;
    struct Yuv {
        device: ID3D11Device,
        textures: Vec<ID3D11Texture2D>,
        identity: SurfaceIdentity,
        format: SurfaceFormat,
        colour: frame_encode::VideoColour,
    }
    impl PayloadOwner for Yuv {
        fn encode_texture(&self, index: usize) -> Option<EncodeTexture> {
            let texture = self.textures.get(index)?;
            Some(EncodeTexture {
                device: self.device.as_raw(),
                texture: texture.as_raw(),
                identity: SurfaceIdentity {
                    texture: texture.as_raw() as usize,
                    ..self.identity
                },
            })
        }
        fn format(&self) -> SurfaceFormat {
            self.format
        }
        fn signal(&self) -> Option<SurfaceSignal> {
            Some(SurfaceSignal {
                colour: self.colour,
                reference_white_nits: 0,
            })
        }
    }
    let output = std::path::PathBuf::from(
        std::env::var_os("FREYM_CODEC_SMOKE_DIR").expect("explicit smoke output directory"),
    );
    std::fs::create_dir_all(&output).unwrap();
    let adapter = super::enumerate_native_adapters()
        .unwrap()
        .into_iter()
        .find(|a| a.vendor_id == 0x10de)
        .expect("local NVIDIA adapter");
    let device = super::native::qualification_device_for_luid(adapter.luid).unwrap();
    let context = unsafe { device.GetImmediateContext() }.unwrap();
    let mt: ID3D11Multithread = context.cast().unwrap();
    let _ = unsafe { mt.SetMultithreadProtected(true) };
    let width = 256;
    let height = 256;
    for (index, profile) in [P::H264High, P::Av1Main8, P::HevcMain10, P::Av1Main10]
        .into_iter()
        .enumerate()
    {
        let ten = profile.bit_depth() == 10;
        let mut colour = if ten {
            frame_encode::VideoColour::BT2020_PQ_LIMITED
        } else {
            frame_encode::VideoColour::BT709_LIMITED
        };
        if profile == P::HevcMain10 {
            colour.range = 2;
        }
        if profile == P::Av1Main10 {
            colour.transfer_characteristics = 18;
        }
        if ten {
            colour.content_light = Some(frame_encode::ContentLight {
                max_cll: 1000,
                max_fall: 400,
            });
            colour.mastering = Some(frame_encode::MasteringDisplay {
                red: (0.68, 0.32),
                green: (0.265, 0.69),
                blue: (0.15, 0.06),
                white: (0.3127, 0.3290),
                luminance_max: 1000.0,
                luminance_min: 0.005,
            });
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: if ten {
                DXGI_FORMAT_P010
            } else {
                DXGI_FORMAT_NV12
            },
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            ..Default::default()
        };
        let mut texture = None;
        let mut bytes = vec![0u8; (width * height * 3 / 2 * if ten { 2 } else { 1 }) as usize];
        if ten {
            for (at, v) in bytes.chunks_exact_mut(2).enumerate() {
                let value: u16 = if at < (width * height) as usize {
                    256 << 6
                } else {
                    512 << 6
                };
                v.copy_from_slice(&value.to_le_bytes());
            }
        } else {
            bytes[..(width * height) as usize].fill(80);
            bytes[(width * height) as usize..].fill(128);
        }
        let data = D3D11_SUBRESOURCE_DATA {
            pSysMem: bytes.as_ptr().cast(),
            SysMemPitch: width * if ten { 2 } else { 1 },
            SysMemSlicePitch: 0,
        };
        unsafe { device.CreateTexture2D(&desc, Some(&data), Some(&mut texture)) }.unwrap();
        let texture = texture.unwrap();
        let pool = SurfacePool::new(bytes.len() as u64 * 4);
        let identity = SurfaceIdentity {
            pool: pool.id(),
            device: device.as_raw() as usize,
            texture: texture.as_raw() as usize,
            width,
            height,
        };
        let mut textures = vec![texture];
        for _ in 1..SLOT_COUNT {
            let mut texture = None;
            unsafe { device.CreateTexture2D(&desc, Some(&data), Some(&mut texture)) }.unwrap();
            textures.push(texture.unwrap());
        }
        let payload: Arc<dyn PayloadOwner> = Arc::new(Yuv {
            device: device.clone(),
            textures,
            identity,
            format: if ten {
                SurfaceFormat::P010
            } else {
                SurfaceFormat::Nv12
            },
            colour,
        });
        let config = NativeEncoderConfig::proof(
            width,
            height,
            NonZeroU32::new(60).unwrap(),
            NonZeroU32::new(1).unwrap(),
        )
        .unwrap();
        let slot: Arc<SurfaceSlot> = pool.try_acquire_copy_slot().unwrap();
        slot.mark_ready();
        slot.mark_consumer_owned();
        let preparation = WindowsSurface::preparation(slot, Arc::clone(&payload));
        let mut encoder =
            WindowsH264Encoder::open_native_profile(config, &preparation, profile).unwrap();
        drop(preparation);
        let slot = pool.try_acquire_copy_slot().unwrap();
        slot.mark_ready();
        slot.mark_consumer_owned();
        let surface = WindowsSurface::new(slot, Arc::clone(&payload));
        assert_eq!(encoder.settings().input_format, if ten { 2 } else { 1 });
        let stream = encoder.info().clone();
        let input = EncoderInput {
            input: CapturedFrame {
                surface,
                timestamp: MediaTimestamp(0),
                width,
                height,
            },
            duration_ns: NonZeroU64::new(16_666_667).unwrap(),
        };
        assert!(matches!(
            encoder.submit(input).unwrap(),
            SubmitOutcome::Accepted
        ));
        encoder.begin_drain().unwrap();
        let packets = collect_packets(&mut encoder, 1, Duration::from_secs(5));
        wait_drained(&mut encoder, Duration::from_secs(5)).unwrap();
        encoder.stop().unwrap();
        let original = output.join(format!("{index}-{:?}.mkv", profile));
        let mut writer =
            frame_mux::MkvVideoWriter::create(&original, &stream, MediaTimestamp(0)).unwrap();
        let mut replay = frame_mux::replay::ReplayBuffer::new(
            stream.clone(),
            vec![],
            frame_mux::replay::ReplayLimit {
                duration_ns: 1_000_000_000,
                max_bytes: 1024 * 1024,
            },
        )
        .unwrap();
        for packet in packets {
            replay.push_video(&packet);
            writer.write(packet).unwrap();
        }
        replay
            .save(&output.join(format!("{index}-replay.mkv")), 16_666_667)
            .unwrap();
        writer.finalize().unwrap();
        frame_mux::recover(&original, &output.join(format!("{index}-recovered.mkv"))).unwrap();
        eprintln!("NATIVE_YUV_SMOKE={}", original.display());
    }
}

const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
const FRAME_PERIOD_NS: u64 = 20_000_000;

// --------------------------------------------------------------- scripted driver

/// How the scripted driver behaves. Tests set fields before or during a run.
#[derive(Default)]
struct Script {
    /// Consecutive busy submit results before the driver accepts.
    busy_submits: usize,
    /// Number of submissions the driver accepts and buffers (NEED_MORE_INPUT): their
    /// pictures are held until a later submission or end of stream, exactly as the SDK
    /// documents.
    buffer_submits: usize,
    /// When true, submitted pictures never complete.
    never_complete: bool,
    /// When true, the newest submission completes before older ones.
    newest_first: bool,
    /// When true, nothing completes until a second submission exists, so a reorder is
    /// guaranteed to be observable.
    hold_for_pair: bool,
    /// When true (the default), end of stream flushes buffered pictures, as the SDK
    /// documents. When false, a buffered picture never produces output.
    flush_buffered_on_eos: bool,
    /// Added to the collected timestamp, to model a driver that changes it.
    pts_offset: i64,
    /// Size of every produced access unit.
    packet_bytes: usize,
    /// Extra busy results for end of stream.
    eos_busy: usize,
    /// When true, end of stream is never acknowledged.
    eos_never: bool,
    /// When true, `close` reports a cleanup failure.
    close_fails: bool,
    /// When true, `unregister_input` fails, so a cleanup cannot count it as released.
    unregister_fails: bool,
    unregister_gate: Option<Arc<std::sync::atomic::AtomicBool>>,
    unregister_entered: bool,
    /// Number of `close` calls that report unconfirmed native completion before they start
    /// succeeding. Models a driver that resolves after delayed completion; a permanently
    /// unresolved driver parks its owner worker instead.
    cleanup_unsafe_attempts: usize,
    /// Sticky mirror of the production bridge: set while a close reported unconfirmed
    /// native completion, cleared only by a successful close.
    cleanup_still_owned: bool,
    cleanup_gate: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// When true, `register_input` fails with an unsupported-codec error.
    register_unsupported: bool,
    /// When true, `submit` fails with an encoder-unavailable error.
    submit_unavailable: bool,
    /// When true, `submit` panics, to model a driver failure inside the worker.
    panic_in_submit: bool,
    /// When true, `collect` reports an oversized access unit.
    oversized: bool,
}

#[derive(Default)]
struct DriverState {
    script: Script,
    registrations: Vec<usize>,
    /// Submitted pictures awaiting completion: (slot, pts, duration, keyframe).
    pending: VecDeque<(u32, u64, u64, bool)>,
    /// Pictures the driver accepted and buffered; they are still mapped and still own
    /// their output slot until the stream advances.
    buffered: VecDeque<(u32, u64, u64, bool)>,
    /// Completed pictures ready to collect.
    ready: VecDeque<(u32, u64, u64, bool)>,
    next_slot: u32,
    /// Set once a pair of submissions has existed, so `hold_for_pair` releases.
    pair_seen: bool,
    eos_submitted: bool,
    eos_acknowledged: bool,
    closes: usize,
    unregisters: usize,
    submissions: Vec<(u64, u64)>,
    collected: Vec<(u64, u64)>,
    max_pending: usize,
}

/// A driver state whose scripted access units are visibly non-empty.
fn driver_state() -> Arc<Mutex<DriverState>> {
    Arc::new(Mutex::new(DriverState {
        script: Script {
            packet_bytes: 256,
            // End of stream flushes buffered pictures unless a test says otherwise.
            flush_buffered_on_eos: true,
            ..Script::default()
        },
        ..DriverState::default()
    }))
}

impl DriverState {
    fn complete_all(&mut self) {
        if self.script.hold_for_pair && !self.pair_seen {
            if self.pending.len() < 2 {
                return;
            }
            self.pair_seen = true;
        }
        while let Some(entry) = self.pending.pop_front() {
            self.ready.push_back(entry);
        }
    }

    /// Completes the newest submission while a second one exists, then the remaining
    /// ones in order, so a reorder is observable exactly once.
    fn complete_newest(&mut self) {
        if self.script.hold_for_pair && !self.pair_seen {
            if self.pending.len() < 2 {
                return;
            }
            self.pair_seen = true;
        }
        let entry = if self.pending.len() >= 2 {
            self.pending.pop_back()
        } else {
            self.pending.pop_front()
        };
        if let Some(entry) = entry {
            self.ready.push_back(entry);
        }
    }
}

/// One scripted native session. It has no GPU, no CUDA and no NVIDIA call.
struct ScriptedDriver {
    settings: NativeEncoderSettings,
    state: Arc<Mutex<DriverState>>,
}

impl ScriptedDriver {
    fn new(settings: NativeEncoderSettings, state: Arc<Mutex<DriverState>>) -> Self {
        Self { settings, state }
    }
}

fn lock(state: &Arc<Mutex<DriverState>>) -> std::sync::MutexGuard<'_, DriverState> {
    state.lock().unwrap_or_else(|poison| poison.into_inner())
}

impl NvencDriver for ScriptedDriver {
    fn settings(&self) -> &NativeEncoderSettings {
        &self.settings
    }

    fn register_input(&mut self, texture: EncodeTexture) -> Result<u32, RecorderError> {
        let mut state = lock(&self.state);
        if state.script.register_unsupported {
            return Err(RecorderError {
                code: ErrorCode::UnsupportedCodec,
                message: "SCRIPTED registration failure".to_string(),
                recovery: frame_contracts::RecoveryStatus::NoRecording,
                action: "not hardware evidence".to_string(),
            });
        }
        if state.registrations.len() >= 4 {
            return Err(internal_error("SCRIPTED registration bound".to_string()));
        }
        state.registrations.push(texture.identity.texture);
        Ok(u32::try_from(state.registrations.len() - 1).unwrap_or(0))
    }

    fn unregister_input(&mut self, _registration: u32) -> Result<(), RecorderError> {
        let gate = {
            let mut state = lock(&self.state);
            state.script.unregister_entered = true;
            state.script.unregister_gate.clone()
        };
        if let Some(gate) = gate {
            while !gate.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let mut state = lock(&self.state);
        if state.script.unregister_fails {
            return Err(internal_error("SCRIPTED unregister failure".to_string()));
        }
        state.unregisters += 1;
        Ok(())
    }

    fn submit(
        &mut self,
        _registration: u32,
        pts_ns: u64,
        duration_ns: u64,
        _force_idr: bool,
    ) -> Result<NativeSubmit, RecorderError> {
        let mut state = lock(&self.state);
        if state.script.panic_in_submit {
            panic!("SCRIPTED driver panic inside submit");
        }
        if state.script.submit_unavailable {
            return Err(RecorderError {
                code: ErrorCode::EncoderUnavailable,
                message: "SCRIPTED submission failure".to_string(),
                recovery: frame_contracts::RecoveryStatus::Unknown,
                action: "not hardware evidence".to_string(),
            });
        }
        if state.script.busy_submits > 0 {
            state.script.busy_submits -= 1;
            return Ok(NativeSubmit::Busy);
        }
        if state.script.buffer_submits > 0 {
            state.script.buffer_submits -= 1;
            let slot = state.next_slot % 4;
            state.next_slot += 1;
            let keyframe = state.submissions.is_empty();
            state.submissions.push((pts_ns, duration_ns));
            state
                .buffered
                .push_back((slot, pts_ns, duration_ns, keyframe));
            state.max_pending = state
                .max_pending
                .max(state.buffered.len() + state.pending.len());
            return Ok(NativeSubmit::Buffered { slot });
        }
        let slot = state.next_slot % 4;
        state.next_slot += 1;
        let keyframe = state.submissions.is_empty();
        state.submissions.push((pts_ns, duration_ns));
        state
            .pending
            .push_back((slot, pts_ns, duration_ns, keyframe));
        state.max_pending = state.max_pending.max(state.pending.len());
        Ok(NativeSubmit::Pending { slot })
    }

    fn collect(
        &mut self,
        slot: u32,
        wait_ms: u32,
        capacity: usize,
    ) -> Result<NativeCollect, RecorderError> {
        let mut state = lock(&self.state);
        if state.script.oversized {
            return Err(internal_error(format!(
                "SCRIPTED access unit exceeds the {capacity}-byte bound"
            )));
        }
        if state.script.never_complete {
            let _ = wait_ms;
            return Ok(NativeCollect::Pending);
        }
        if state.script.newest_first {
            state.complete_newest();
        } else {
            state.complete_all();
        }
        let Some((_, pts, duration, keyframe)) = state
            .ready
            .iter()
            .position(|entry| entry.0 == slot)
            .map(|index| {
                state
                    .ready
                    .remove(index)
                    .expect("the index came from this queue")
            })
        else {
            return Ok(NativeCollect::Pending);
        };
        let adjusted = i64::try_from(pts).unwrap_or(i64::MAX) + state.script.pts_offset;
        let pts_ns = u64::try_from(adjusted).unwrap_or(0);
        let length = state.script.packet_bytes;
        state.collected.push((pts_ns, duration));
        Ok(NativeCollect::Ready(NativePacket {
            data: vec![0x5A; length],
            pts_ns,
            duration_ns: duration,
            keyframe,
            picture_type: if keyframe { 3 } else { 2 },
            frame_index: state.collected.len() as u64 - 1,
        }))
    }

    fn begin_eos(&mut self) -> Result<NativeEos, RecorderError> {
        let mut state = lock(&self.state);
        if state.script.eos_busy > 0 {
            state.script.eos_busy -= 1;
            return Ok(NativeEos::Busy);
        }
        state.eos_submitted = true;
        // End of stream flushes the pictures the driver buffered, which is what the SDK
        // documents and what the corrected EOS timing relies on.
        if state.script.flush_buffered_on_eos {
            while let Some(entry) = state.buffered.pop_front() {
                state.pending.push_back(entry);
            }
        }
        Ok(NativeEos::Submitted)
    }

    fn poll_eos(&mut self, _wait_ms: u32) -> Result<bool, RecorderError> {
        let mut state = lock(&self.state);
        if state.script.eos_never {
            return Ok(false);
        }
        state.eos_acknowledged = true;
        Ok(true)
    }

    fn stats(&self) -> NativeStats {
        let state = lock(&self.state);
        NativeStats {
            native_calls: u32::try_from(state.submissions.len() + state.collected.len())
                .unwrap_or(0),
            busy_results: 0,
            pending_inputs: u32::try_from(state.pending.len()).unwrap_or(0),
            ..Default::default()
        }
    }

    fn cleanup_unsafe(&self) -> bool {
        lock(&self.state).script.cleanup_still_owned
    }

    fn close(&mut self) -> Result<(), RecorderError> {
        let mut state = lock(&self.state);
        state.closes += 1;
        if state
            .script
            .cleanup_gate
            .as_ref()
            .is_some_and(|gate| !gate.load(Ordering::Acquire))
        {
            state.script.cleanup_still_owned = true;
            return Err(internal_error("SCRIPTED gated cleanup".to_string()));
        }
        if state.script.cleanup_unsafe_attempts > 0 {
            state.script.cleanup_unsafe_attempts -= 1;
            state.script.cleanup_still_owned = true;
            return Err(internal_error(
                "SCRIPTED cleanup could not confirm native completion".to_string(),
            ));
        }
        state.script.cleanup_still_owned = false;
        if state.script.close_fails {
            return Err(internal_error("SCRIPTED close failure".to_string()));
        }
        Ok(())
    }
}

// ------------------------------------------------------------- synthetic capture

/// A synthetic payload owner whose textures are distinct per slot, so the encoder's
/// pool/device/texture checks are exercised without a GPU.
struct TestPayload {
    lifetime: Arc<PayloadLifetime>,
    pool: u64,
    device: usize,
    base_texture: usize,
    drifting: bool,
    calls: Mutex<usize>,
    frame: Arc<Mutex<Option<Weak<PayloadLifetime>>>>,
}

impl TestPayload {
    fn new(
        lifetime: &Arc<PayloadLifetime>,
        pool: u64,
        device: usize,
        base_texture: usize,
        drifting: bool,
    ) -> Arc<Self> {
        lifetime.owner_created();
        let _ = &lifetime;
        Arc::new(Self {
            lifetime: Arc::clone(lifetime),
            pool,
            device,
            base_texture,
            drifting,
            calls: Mutex::new(0),
            frame: Arc::new(Mutex::new(None)),
        })
    }
}

impl PayloadOwner for TestPayload {
    fn encode_texture(&self, slot_index: usize) -> Option<EncodeTexture> {
        if slot_index >= SLOT_COUNT {
            return None;
        }
        let texture = if self.drifting {
            let mut calls = self
                .calls
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            *calls += 1;
            self.base_texture + *calls * 0x100
        } else {
            self.base_texture + slot_index * 0x100
        };
        Some(EncodeTexture {
            device: std::ptr::without_provenance_mut(self.device),
            texture: std::ptr::without_provenance_mut(texture),
            identity: SurfaceIdentity {
                pool: self.pool,
                device: self.device,
                texture,
                width: WIDTH,
                height: HEIGHT,
            },
        })
    }
}

impl Drop for TestPayload {
    fn drop(&mut self) {
        self.lifetime.owner_dropped();
        if let Ok(mut slot) = self.frame.lock() {
            *slot = None;
        }
    }
}

/// One synthetic capture pool plus the payload owner its leases retain.
struct TestSource {
    pool: Arc<SurfacePool>,
    payload: Arc<dyn PayloadOwner>,
    identity: SurfaceIdentity,
    weak: Weak<TestPayload>,
}

impl TestSource {
    fn new(device: usize, drifting: bool) -> Self {
        let lifetime = PayloadLifetime::new();
        let pool = SurfacePool::new(
            plan_owned_payload(WIDTH, HEIGHT, false)
                .expect("the synthetic payload fits")
                .slots_bytes,
        );
        let payload = TestPayload::new(&lifetime, pool.id(), device, 0x4000, drifting);
        let weak = Arc::downgrade(&payload);
        let identity = SurfaceIdentity {
            pool: pool.id(),
            device,
            texture: 0x4000,
            width: WIDTH,
            height: HEIGHT,
        };
        Self {
            pool,
            payload,
            identity,
            weak,
        }
    }

    /// Takes one lease from the pool, exactly as the capture session would.
    fn take(&self, pts_ns: u64) -> CapturedFrame<WindowsSurface> {
        self.take_at(pts_ns, WIDTH, HEIGHT)
    }

    fn take_at(&self, pts_ns: u64, width: u32, height: u32) -> CapturedFrame<WindowsSurface> {
        let slot = self
            .pool
            .try_acquire_copy_slot()
            .expect("the synthetic pool has a free slot");
        slot.mark_ready();
        slot.mark_consumer_owned();
        CapturedFrame {
            surface: WindowsSurface::new(slot, Arc::clone(&self.payload)),
            timestamp: MediaTimestamp(pts_ns),
            width,
            height,
        }
    }

    fn payload_alive(&self) -> bool {
        self.weak.upgrade().is_some()
    }

    fn free_slots(&self) -> usize {
        self.pool.counts().free
    }
}

// ---------------------------------------------------------------------- harness

fn settings() -> NativeEncoderSettings {
    NativeEncoderSettings {
        advanced: Default::default(),
        codec: 0,
        input_format: 0,
        caps_input_formats: 1,
        caps_profile: true,
        caps_10bit: false,
        width: WIDTH,
        height: HEIGHT,
        rate_num: 60,
        rate_den: 1,
        bitrate_bps: 40_000_000,
        max_bitrate_bps: 40_000_000,
        gop_length: 120,
        idr_period: 120,
        frame_interval_p: 1,
        enable_ptd: true,
        enable_async: true,
        repeat_spspps: true,
        level: 0,
        profile_high: true,
        vui_full_range: false,
        rate_control: NativeRateControl::Cbr,
        preset: NativeEncoderPreset::LowLatency,
        caps_async_encode: true,
        caps_width_max: 4096,
        caps_height_max: 4096,
        caps_rate_control_modes: 0x2,
        caps_h264_guid: true,
        caps_input_argb: true,
        caps_profile_high: true,
        caps_preset_available: true,
        output_buffers: 4,
        output_buffer_capacity: MAX_ACCESS_UNIT_BYTES + 65_536,
        max_access_unit_bytes: MAX_ACCESS_UNIT_BYTES,
        codec_config: vec![0, 0, 0, 1, 0x67, 0x64, 0x00, 0x1F],
    }
}

fn config() -> NativeEncoderConfig {
    NativeEncoderConfig::proof(
        WIDTH,
        HEIGHT,
        NonZeroU32::new(60).expect("non-zero"),
        NonZeroU32::new(1).expect("non-zero"),
    )
    .expect("the synthetic configuration is valid")
}

/// Builds the production facade around a scripted driver.
fn open_encoder(source: &TestSource, state: &Arc<Mutex<DriverState>>) -> WindowsH264Encoder {
    let driver = ScriptedDriver::new(settings(), Arc::clone(state));
    WindowsH264Encoder::with_driver(
        config(),
        source.identity,
        Box::new(driver),
        Arc::clone(&source.payload),
    )
    .expect("the scripted session opens")
}

fn duration() -> NonZeroU64 {
    NonZeroU64::new(FRAME_PERIOD_NS).expect("non-zero")
}

#[test]
fn prepared_encoder_registers_the_fixed_pool_before_readiness_and_refuses_unfilled_input() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    let driver_state = Arc::clone(&state);
    let mut encoder = WindowsH264Encoder::start_with_prepared(
        config(),
        source.identity,
        Arc::clone(&source.payload),
        true,
        move || Ok(Box::new(ScriptedDriver::new(settings(), driver_state))),
    )
    .unwrap();
    assert_eq!(lock(&state).registrations.len(), SLOT_COUNT);
    assert!(lock(&state).submissions.is_empty());
    assert_eq!(
        source.free_slots(),
        SLOT_COUNT,
        "registration retains backing, not leases"
    );
    let slot = source.pool.try_acquire_copy_slot().unwrap();
    slot.mark_consumer_owned();
    let error = encoder
        .submit(EncoderInput {
            input: CapturedFrame {
                surface: WindowsSurface::preparation(slot, Arc::clone(&source.payload)),
                timestamp: MediaTimestamp(1),
                width: WIDTH,
                height: HEIGHT,
            },
            duration_ns: duration(),
        })
        .err()
        .expect("unfilled input must be refused");
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert_eq!(encoder.counters().accepted_inputs, 0);
    assert_eq!(source.free_slots(), SLOT_COUNT);
    submit(&mut encoder, &source, 1).unwrap();
    assert_eq!(
        collect_packets(&mut encoder, 1, Duration::from_secs(2)).len(),
        1
    );
    encoder.begin_drain().unwrap();
    wait_drained(&mut encoder, Duration::from_secs(2)).unwrap();
    encoder.stop().unwrap();
    assert_eq!(lock(&state).unregisters, SLOT_COUNT);
}

#[test]
fn prepared_pool_registration_failure_cleans_up_before_reporting_readiness() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    lock(&state).script.register_unsupported = true;
    let driver_state = Arc::clone(&state);
    let error = WindowsH264Encoder::start_with_prepared(
        config(),
        source.identity,
        Arc::clone(&source.payload),
        true,
        move || Ok(Box::new(ScriptedDriver::new(settings(), driver_state))),
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::UnsupportedCodec);
    assert_eq!(lock(&state).closes, 1);
    assert_eq!(source.free_slots(), SLOT_COUNT);
}

fn submit(
    encoder: &mut WindowsH264Encoder,
    source: &TestSource,
    pts_ns: u64,
) -> Result<SubmitOutcome<EncoderInput<CapturedFrame<WindowsSurface>>>, RecorderError> {
    encoder.submit(EncoderInput {
        input: source.take(pts_ns),
        duration_ns: duration(),
    })
}

/// Collects packets until `wanted` have arrived or the deadline passes.
fn collect_packets(
    encoder: &mut WindowsH264Encoder,
    wanted: usize,
    timeout: Duration,
) -> Vec<frame_encode::EncodedPacket> {
    let mut packets = Vec::new();
    let deadline = Instant::now() + timeout;
    while packets.len() < wanted && Instant::now() < deadline {
        match encoder.try_packet() {
            Ok(EncoderPoll::Packet(packet)) => packets.push(packet),
            Ok(EncoderPoll::Pending) => std::thread::sleep(Duration::from_millis(1)),
            Ok(EncoderPoll::Drained) => break,
            Err(error) => panic!("the scripted session must not fail: {error}"),
        }
    }
    packets
}

/// Waits for `Drained`, failing on an error or a timeout.
fn wait_drained(encoder: &mut WindowsH264Encoder, timeout: Duration) -> Result<(), RecorderError> {
    let deadline = Instant::now() + timeout;
    loop {
        match encoder.try_packet()? {
            EncoderPoll::Packet(_) => panic!("a packet was still queued while waiting for Drained"),
            EncoderPoll::Pending => {
                if Instant::now() >= deadline {
                    return Err(internal_error("SCRIPTED drain timed out".to_string()));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            EncoderPoll::Drained => return Ok(()),
        }
    }
}

// ------------------------------------------------------------------------ tests

#[test]
fn drain_cannot_overtake_an_input_published_after_the_empty_channel_observation() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    let mut encoder = open_encoder(&source, &state);
    let (arrived, arrival) = std::sync::mpsc::channel();
    let (release, resume) = std::sync::mpsc::channel();
    *encoder.shared.before_eos_check.lock().unwrap() = Some(Box::new(move || {
        arrived.send(()).unwrap();
        resume.recv_timeout(Duration::from_secs(3)).unwrap();
    }));
    arrival.recv_timeout(Duration::from_secs(2)).unwrap();
    // The worker has observed an empty ingress, but it has not read drain yet.
    let submitted = submit(&mut encoder, &source, 1_000);
    let drain = encoder.begin_drain();
    release.send(()).unwrap();
    // Release the worker before asserting, including on an unexpected result.
    assert!(matches!(submitted.unwrap(), SubmitOutcome::Accepted));
    drain.unwrap();
    let packets = collect_packets(&mut encoder, 1, Duration::from_secs(2));
    assert_eq!(packets.len(), 1, "EOS must follow the accepted frame");
    assert_eq!(packets[0].pts, MediaTimestamp(1_000));
    wait_drained(&mut encoder, Duration::from_secs(2)).unwrap();
    encoder.stop().unwrap();
    assert_eq!(encoder.counters().queued_inputs, 0);
}

#[test]
fn pressure_returns_the_exact_owned_input_and_keeps_every_lease_bounded() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    lock(&state).script.never_complete = true;
    let mut encoder = open_encoder(&source, &state);

    // The scripted driver never completes, so the encoder retains the maximum three
    // leases and then returns the fourth input untouched.
    for index in 0..MAX_ENCODER_LEASES {
        let outcome = submit(&mut encoder, &source, FRAME_PERIOD_NS * (index as u64 + 1))
            .expect("admission must not fail");
        assert!(matches!(outcome, SubmitOutcome::Accepted), "frame {index}");
    }
    let fourth = source.take(FRAME_PERIOD_NS * 10);
    let fourth_pts = fourth.timestamp;
    let outcome = encoder
        .submit(EncoderInput {
            input: fourth,
            duration_ns: duration(),
        })
        .expect("pressure is not an error");
    match outcome {
        SubmitOutcome::Backpressure(returned) => {
            assert_eq!(
                returned.input.timestamp, fourth_pts,
                "Backpressure must return the exact owned input"
            );
            assert_eq!(returned.duration_ns, duration());
        }
        SubmitOutcome::Accepted => panic!("a fourth retained lease must be refused"),
    }
    let counters = encoder.counters();
    assert_eq!(counters.accepted_inputs, MAX_ENCODER_LEASES as u64);
    assert_eq!(counters.backpressure_returns, 1);
    assert_eq!(counters.retained_leases, MAX_ENCODER_LEASES as u64);
    assert!(counters.identities().is_ok(), "{:?}", counters.identities());

    // That input is dropped (its lease returns to the pool), and once the driver
    // completes its work the encoder accepts it again.
    assert_eq!(source.free_slots(), 1);
    lock(&state).script.never_complete = false;
    lock(&state).complete_all();
    let _ = collect_packets(&mut encoder, MAX_ENCODER_LEASES, Duration::from_secs(2));
    let deadline = Instant::now() + Duration::from_secs(2);
    while encoder.counters().retained_leases != 0 && Instant::now() < deadline {
        match encoder.try_packet() {
            Ok(_) => std::thread::sleep(Duration::from_millis(1)),
            Err(error) => panic!("unexpected error: {error}"),
        }
    }
    assert_eq!(encoder.counters().retained_leases, 0);
    assert_eq!(
        source.free_slots(),
        SLOT_COUNT,
        "every lease must return to its own pool"
    );
    assert!(encoder.counters().identities().is_ok());
}

#[test]
fn a_buffered_input_stays_owned_and_is_delivered_at_end_of_stream() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    lock(&state).script.buffer_submits = 1;
    let mut encoder = open_encoder(&source, &state);

    assert!(matches!(
        submit(&mut encoder, &source, FRAME_PERIOD_NS).expect("admission"),
        SubmitOutcome::Accepted
    ));
    // The buffered picture is outstanding: the lease is retained, its slot is owned and no
    // packet exists yet. It is not a completed submission and never a released input.
    let deadline = Instant::now() + Duration::from_secs(2);
    while encoder.counters().native_buffered == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    let counters = encoder.counters();
    assert_eq!(counters.native_buffered, 1);
    assert_eq!(counters.packets_emitted, 0);
    assert_eq!(
        counters.retained_leases, 1,
        "the buffered picture still owns its lease"
    );
    assert_eq!(counters.submitted_in_flight(), 1);
    assert_eq!(source.free_slots(), SLOT_COUNT - 1);
    assert!(counters.identities().is_ok(), "{:?}", counters.identities());

    // End of stream flushes it: one accepted input must become exactly one delivered
    // packet, and only then may the lease be released.
    encoder.begin_drain().expect("drain is out of band");
    let packets = collect_packets(&mut encoder, 1, Duration::from_secs(3));
    assert_eq!(packets.len(), 1, "the buffered picture must be delivered");
    assert_eq!(packets[0].pts.0, FRAME_PERIOD_NS);
    assert_eq!(packets[0].duration_ns, FRAME_PERIOD_NS);
    wait_drained(&mut encoder, Duration::from_secs(3)).expect("the session must drain");
    encoder.stop().expect("a completed drain stops cleanly");
    let counters = encoder.counters();
    assert_eq!(counters.accepted_inputs, 1);
    assert_eq!(counters.native_buffered, 1);
    assert_eq!(counters.packets_emitted, 1);
    assert_eq!(counters.packets_dequeued, 1);
    assert_eq!(counters.retained_leases, 0);
    assert_eq!(source.free_slots(), SLOT_COUNT);
    assert!(counters.identities().is_ok(), "{:?}", counters.identities());
}

#[test]
fn a_buffered_picture_that_never_completes_is_an_explicit_failure_not_a_lost_input() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    lock(&state).script.buffer_submits = 1;
    lock(&state).script.flush_buffered_on_eos = false;
    let mut encoder = open_encoder(&source, &state);
    assert!(matches!(
        submit(&mut encoder, &source, FRAME_PERIOD_NS).expect("admission"),
        SubmitOutcome::Accepted
    ));
    let deadline = Instant::now() + Duration::from_secs(2);
    while encoder.counters().native_buffered == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    encoder.begin_drain().expect("drain");
    // The drain cannot complete: the accepted input never became a packet, so the session
    // reports the failure instead of balancing the loss away.
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut observed = None;
    while observed.is_none() && Instant::now() < deadline {
        match encoder.try_packet() {
            Ok(_) => std::thread::sleep(Duration::from_millis(2)),
            Err(error) => observed = Some(error),
        }
    }
    let error = observed.expect("a buffered picture that never completes must fail the session");
    assert_eq!(error.code, ErrorCode::Internal);
    assert_eq!(encoder.counters().packets_emitted, 0);
    assert_eq!(encoder.counters().accepted_inputs, 1);
    assert_eq!(
        encoder.counters().retained_leases,
        0,
        "the lease is released after native close"
    );
    assert_eq!(source.free_slots(), SLOT_COUNT);
    assert!(encoder.counters().identities().is_ok());
}
#[test]
fn zero_one_and_several_frames_all_drain_without_further_input() {
    for frames in [0usize, 1, 3] {
        let source = TestSource::new(0x1000, false);
        let state = driver_state();
        let mut encoder = open_encoder(&source, &state);
        for index in 0..frames {
            assert!(
                matches!(
                    submit(&mut encoder, &source, FRAME_PERIOD_NS * (index as u64 + 1))
                        .expect("admission"),
                    SubmitOutcome::Accepted
                ),
                "{frames} frames, index {index}"
            );
        }
        encoder.begin_drain().expect("drain");
        let packets = collect_packets(&mut encoder, frames, Duration::from_secs(2));
        assert_eq!(packets.len(), frames, "{frames} frames");
        for (index, packet) in packets.iter().enumerate() {
            assert_eq!(packet.pts.0, FRAME_PERIOD_NS * (index as u64 + 1));
            assert_eq!(packet.dts, packet.pts, "DTS must equal PTS");
            assert_eq!(packet.duration_ns, FRAME_PERIOD_NS);
            assert_eq!(
                packet.keyframe,
                index == 0,
                "only the first frame is an IDR"
            );
            assert!(!packet.data.is_empty());
        }
        wait_drained(&mut encoder, Duration::from_secs(2)).expect("drain completes");
        assert!(encoder.drained());
        // Drained is stable on repeated polls.
        assert!(matches!(
            encoder.try_packet().expect("stable drain"),
            EncoderPoll::Drained
        ));
        // Admission is closed after drain.
        let refused = match submit(&mut encoder, &source, FRAME_PERIOD_NS * 100) {
            Err(error) => error,
            Ok(_) => panic!("admission after drain must be refused"),
        };
        assert_eq!(refused.code, ErrorCode::InvalidRequest);
        encoder.stop().expect("a completed drain stops cleanly");
        let counters = encoder.counters();
        assert_eq!(counters.eos_submissions, 1);
        assert_eq!(counters.eos_acknowledgements, 1);
        assert_eq!(counters.retained_leases, 0);
        assert_eq!(counters.registrations, counters.unregistrations);
        assert!(counters.identities().is_ok(), "{:?}", counters.identities());
    }
}

#[test]
fn pending_is_never_drained_and_end_of_stream_is_acknowledged_once() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    lock(&state).script.eos_never = true;
    let mut encoder = open_encoder(&source, &state);
    assert!(matches!(
        submit(&mut encoder, &source, FRAME_PERIOD_NS).expect("admission"),
        SubmitOutcome::Accepted
    ));
    let _ = collect_packets(&mut encoder, 1, Duration::from_secs(2));
    encoder.begin_drain().expect("drain");
    for _ in 0..50 {
        assert!(
            matches!(
                encoder.try_packet().expect("no failure yet"),
                EncoderPoll::Pending
            ),
            "an unacknowledged end of stream is Pending, never Drained"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!encoder.drained());
    // An early stop must still clean up and report the incomplete finalization.
    let error = encoder
        .stop()
        .expect_err("an unacknowledged drain cannot stop cleanly");
    assert_eq!(error.code, ErrorCode::FinalizationFailed);
    assert!(encoder.counters().identities().is_ok());
    assert_eq!(encoder.counters().retained_leases, 0);
}

#[test]
fn begin_drain_is_idempotent_and_reports_a_sticky_terminal_error() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    let mut encoder = open_encoder(&source, &state);
    encoder.begin_drain().expect("first drain");
    encoder.begin_drain().expect("repeated drain is idempotent");
    encoder.begin_drain().expect("repeated drain is idempotent");
    assert_eq!(encoder.counters().drain_requests, 1);
    wait_drained(&mut encoder, Duration::from_secs(2)).expect("zero-frame drain");
    encoder.stop().expect("clean stop");

    // A failing driver session is sticky and is reported by every entry point.
    let failing = TestSource::new(0x1000, false);
    let failing_state = driver_state();
    lock(&failing_state).script.submit_unavailable = true;
    let mut failing_encoder = open_encoder(&failing, &failing_state);
    assert!(matches!(
        submit(&mut failing_encoder, &failing, FRAME_PERIOD_NS).expect("admission"),
        SubmitOutcome::Accepted
    ));
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut observed = None;
    while observed.is_none() && Instant::now() < deadline {
        match failing_encoder.try_packet() {
            Ok(_) => std::thread::sleep(Duration::from_millis(1)),
            Err(error) => observed = Some(error),
        }
    }
    let error = observed.expect("a native submission failure must surface");
    assert_eq!(error.code, ErrorCode::EncoderUnavailable);
    let again = failing_encoder
        .try_packet()
        .expect_err("the terminal error is sticky");
    assert_eq!(again.code, ErrorCode::EncoderUnavailable);
    assert_eq!(
        failing_encoder
            .begin_drain()
            .expect_err("drain after a terminal error")
            .code,
        ErrorCode::EncoderUnavailable
    );
    assert_eq!(
        failing_encoder
            .stop()
            .expect_err("stop reports the terminal error")
            .code,
        ErrorCode::EncoderUnavailable
    );
    let counters = failing_encoder.counters();
    assert_eq!(counters.native_failures, 1);
    assert_eq!(counters.retained_leases, 0);
    assert!(counters.identities().is_ok(), "{:?}", counters.identities());
}

#[test]
fn a_stop_before_drain_is_an_explicit_incomplete_finalization() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    let mut encoder = open_encoder(&source, &state);
    assert!(matches!(
        submit(&mut encoder, &source, FRAME_PERIOD_NS).expect("admission"),
        SubmitOutcome::Accepted
    ));
    let error = encoder
        .stop()
        .expect_err("stopping without a drain cannot report success");
    assert_eq!(error.code, ErrorCode::FinalizationFailed);
    let counters = encoder.counters();
    assert_eq!(counters.accepted_inputs, 1);
    assert_eq!(
        counters.inputs_discarded_on_stop + counters.native_submissions,
        1
    );
    assert_eq!(counters.retained_leases, 0);
    assert_eq!(counters.registrations, counters.unregistrations);
    assert!(counters.identities().is_ok(), "{:?}", counters.identities());
    assert_eq!(source.free_slots(), SLOT_COUNT);
    // Repeated stop returns the same result and does not run teardown twice.
    assert_eq!(
        encoder.stop().expect_err("stop stays failed").code,
        ErrorCode::FinalizationFailed
    );
    assert_eq!(lock(&state).closes, 1, "the native session is closed once");
}

#[test]
fn a_queued_packet_that_is_never_consumed_is_discarded_explicitly_on_stop() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    let mut encoder = open_encoder(&source, &state);
    for index in 0..2 {
        assert!(matches!(
            submit(&mut encoder, &source, FRAME_PERIOD_NS * (index as u64 + 1)).expect("admission"),
            SubmitOutcome::Accepted
        ));
    }
    encoder.begin_drain().expect("drain");
    let deadline = Instant::now() + Duration::from_secs(2);
    while encoder.counters().eos_acknowledgements == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(encoder.counters().eos_acknowledgements, 1);
    // Two packets are queued but never consumed: the drain itself is acknowledged,
    // while stop reports that they were never delivered.
    let error = encoder
        .stop()
        .expect_err("undelivered packets must fail finalization");
    assert_eq!(error.code, ErrorCode::FinalizationFailed);
    let counters = encoder.counters();
    assert_eq!(counters.packets_discarded_on_stop, 2);
    assert_eq!(counters.packets_held, 0);
    assert!(counters.identities().is_ok(), "{:?}", counters.identities());
}

#[test]
fn oversized_access_units_and_packet_bounds_are_never_violated() {
    // A driver that reports more than the accepted access-unit bound fails explicitly.
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    lock(&state).script.oversized = true;
    let mut encoder = open_encoder(&source, &state);
    assert!(matches!(
        submit(&mut encoder, &source, FRAME_PERIOD_NS).expect("admission"),
        SubmitOutcome::Accepted
    ));
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut observed = None;
    while observed.is_none() && Instant::now() < deadline {
        match encoder.try_packet() {
            Ok(_) => std::thread::sleep(Duration::from_millis(1)),
            Err(error) => observed = Some(error),
        }
    }
    let error = observed.expect("an oversized access unit must fail");
    assert_eq!(error.code, ErrorCode::Internal);
    let counters = encoder.counters();
    assert_eq!(counters.packets_emitted, 0);
    assert_eq!(counters.slots_abandoned_on_failure, 1);
    assert_eq!(counters.retained_leases, 0);
    assert!(counters.identities().is_ok(), "{:?}", counters.identities());

    // A large but legal access unit stays inside every bound.
    let big_source = TestSource::new(0x1000, false);
    let big_state = driver_state();
    lock(&big_state).script.packet_bytes = MAX_ACCESS_UNIT_BYTES as usize;
    let mut big = open_encoder(&big_source, &big_state);
    for index in 0..MAX_ENCODER_LEASES {
        assert!(matches!(
            submit(&mut big, &big_source, FRAME_PERIOD_NS * (index as u64 + 1)).expect("admission"),
            SubmitOutcome::Accepted
        ));
    }
    // The fourth lease belongs to the proof caller's lookahead frame, so a fourth
    // submission is refused rather than retained.
    assert!(matches!(
        submit(&mut big, &big_source, FRAME_PERIOD_NS * 100).expect("pressure is not an error"),
        SubmitOutcome::Backpressure(_)
    ));
    big.begin_drain().expect("drain");
    let packets = collect_packets(&mut big, MAX_ENCODER_LEASES, Duration::from_secs(3));
    assert_eq!(packets.len(), MAX_ENCODER_LEASES);
    let counters = big.counters();
    assert!(counters.bytes_held <= MAX_PACKET_BYTES);
    assert!(counters.packets_held <= MAX_PACKETS as u64);
    assert!(counters.queue_high_water <= MAX_PACKETS as u64);
    assert!(counters.lease_high_water <= MAX_ENCODER_LEASES as u64);
    assert!(big.drained());
    assert!(counters.identities().is_ok(), "{:?}", counters.identities());
    big.stop().expect("clean stop");
}

#[test]
fn native_timestamps_must_survive_the_encode_unchanged() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    lock(&state).script.pts_offset = 1_000;
    let mut encoder = open_encoder(&source, &state);
    assert!(matches!(
        submit(&mut encoder, &source, FRAME_PERIOD_NS).expect("admission"),
        SubmitOutcome::Accepted
    ));
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut observed = None;
    while observed.is_none() && Instant::now() < deadline {
        match encoder.try_packet() {
            Ok(_) => std::thread::sleep(Duration::from_millis(1)),
            Err(error) => observed = Some(error),
        }
    }
    let error = observed.expect("a changed timestamp must fail the session");
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(
        error.message.contains("did not survive"),
        "{}",
        error.message
    );
    let counters = encoder.counters();
    assert_eq!(counters.timestamp_mismatches, 1);
    assert_eq!(counters.packets_emitted, 0);
    assert!(counters.identities().is_ok(), "{:?}", counters.identities());
}

#[test]
fn ready_events_are_collected_in_submission_order_without_rewriting_timestamps() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    lock(&state).script.newest_first = true;
    lock(&state).script.hold_for_pair = true;
    let mut encoder = open_encoder(&source, &state);
    for index in 1..=2 {
        assert!(matches!(
            submit(&mut encoder, &source, FRAME_PERIOD_NS * index).unwrap(),
            SubmitOutcome::Accepted
        ));
    }
    let packets = collect_packets(&mut encoder, 2, Duration::from_secs(2));
    assert_eq!(
        packets.iter().map(|p| p.pts.0).collect::<Vec<_>>(),
        vec![FRAME_PERIOD_NS, FRAME_PERIOD_NS * 2]
    );
    assert_eq!(encoder.counters().timestamp_regressions, 0);
    encoder.begin_drain().unwrap();
    wait_drained(&mut encoder, Duration::from_secs(2)).unwrap();
    encoder.stop().unwrap();
    assert!(encoder.counters().identities().is_ok());
}

#[test]
fn a_stalled_native_completion_is_bounded_by_the_progress_watchdog() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    lock(&state).script.never_complete = true;
    let mut encoder = open_encoder(&source, &state);
    assert!(matches!(
        submit(&mut encoder, &source, FRAME_PERIOD_NS).expect("admission"),
        SubmitOutcome::Accepted
    ));
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut observed = None;
    while observed.is_none() && Instant::now() < deadline {
        match encoder.try_packet() {
            Ok(_) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => observed = Some(error),
        }
    }
    let error = observed.expect("a stalled driver must be bounded by the watchdog");
    assert!(
        error.message.contains("did not complete"),
        "{}",
        error.message
    );
    let counters = encoder.counters();
    assert_eq!(
        counters.retained_leases, 0,
        "the abandoned lease is released"
    );
    assert!(counters.identities().is_ok(), "{:?}", counters.identities());
    assert_eq!(source.free_slots(), SLOT_COUNT);
}

#[test]
fn an_unwinding_worker_releases_every_lease_and_reports_sticky_failure() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    lock(&state).script.panic_in_submit = true;
    let mut encoder = open_encoder(&source, &state);
    assert!(matches!(
        submit(&mut encoder, &source, FRAME_PERIOD_NS).expect("admission"),
        SubmitOutcome::Accepted
    ));
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut observed = None;
    while observed.is_none() && Instant::now() < deadline {
        match encoder.try_packet() {
            Ok(_) => std::thread::sleep(Duration::from_millis(1)),
            Err(error) => observed = Some(error),
        }
    }
    let error = observed.expect("the unwinding worker must publish a sticky error");
    assert!(
        encoder.native_failure().is_none(),
        "unwind cannot authorize native backend reuse"
    );
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(error.message.contains("unwound"), "{}", error.message);
    // Teardown joins the panicked worker and reports the same terminal condition.
    assert!(encoder.stop().is_err());
    let counters = encoder.counters();
    assert_eq!(counters.inputs_discarded_on_unwind, 1);
    assert_eq!(counters.retained_leases, 0);
    assert_eq!(source.free_slots(), SLOT_COUNT, "the capture slot returns");
    assert_eq!(lock(&state).closes, 1, "the native session is closed once");
    assert!(counters.identities().is_ok(), "{:?}", counters.identities());
}

#[test]
fn a_held_lease_keeps_its_capture_slot_until_native_completion() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    lock(&state).script.never_complete = true;
    let mut encoder = open_encoder(&source, &state);
    assert!(matches!(
        submit(&mut encoder, &source, FRAME_PERIOD_NS).expect("admission"),
        SubmitOutcome::Accepted
    ));
    // Wait until the picture is in flight, then observe that the slot is still owned.
    let deadline = Instant::now() + Duration::from_secs(2);
    while encoder.counters().native_submissions == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(encoder.counters().native_submissions, 1);
    assert_eq!(
        source.free_slots(),
        SLOT_COUNT - 1,
        "the lease is still held"
    );
    assert_eq!(encoder.counters().retained_leases, 1);
    // Completion releases it.
    lock(&state).script.never_complete = false;
    let _ = collect_packets(&mut encoder, 1, Duration::from_secs(2));
    let deadline = Instant::now() + Duration::from_secs(2);
    while encoder.counters().retained_leases != 0 && Instant::now() < deadline {
        match encoder.try_packet() {
            Ok(_) => std::thread::sleep(Duration::from_millis(1)),
            Err(error) => panic!("unexpected error: {error}"),
        }
    }
    assert_eq!(encoder.counters().retained_leases, 0);
    assert_eq!(source.free_slots(), SLOT_COUNT);
    assert!(source.payload_alive());
}

#[test]
fn cross_pool_cross_device_and_cross_dimension_input_is_rejected() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    let mut encoder = open_encoder(&source, &state);

    // Another pool and another device are both rejected terminally.
    let foreign = TestSource::new(0x9000, false);
    assert!(matches!(
        submit(&mut encoder, &foreign, FRAME_PERIOD_NS).expect("admission"),
        SubmitOutcome::Accepted
    ));
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut observed = None;
    while observed.is_none() && Instant::now() < deadline {
        match encoder.try_packet() {
            Ok(_) => std::thread::sleep(Duration::from_millis(1)),
            Err(error) => observed = Some(error),
        }
    }
    let error = observed.expect("foreign input must fail");
    assert!(error.message.contains("cross-pool"), "{}", error.message);
    assert_eq!(encoder.counters().rejected_inputs, 1);
    assert!(encoder.counters().identities().is_ok());
    let _ = encoder.stop();

    // A different geometry for the same device and pool is also rejected.
    let sized = TestSource::new(0x1000, false);
    let sized_state = driver_state();
    let mut sized_encoder = open_encoder(&sized, &sized_state);
    let frame = sized.take_at(FRAME_PERIOD_NS, WIDTH + 2, HEIGHT);
    let outcome = match sized_encoder.submit(EncoderInput {
        input: frame,
        duration_ns: duration(),
    }) {
        Err(error) => error,
        Ok(_) => panic!("the facade rejects a mismatched frame before admission"),
    };
    assert_eq!(outcome.code, ErrorCode::InvalidRequest);
    assert!(
        outcome.message.contains("cannot be submitted"),
        "{}",
        outcome.message
    );
}

#[test]
fn a_fifth_capture_texture_is_rejected_and_registrations_stay_bounded() {
    let source = TestSource::new(0x1000, true);
    let state = driver_state();
    let mut encoder = open_encoder(&source, &state);
    // Five distinct textures from the same pool and device: the fifth is refused.
    for index in 0..5u64 {
        let outcome = submit(&mut encoder, &source, FRAME_PERIOD_NS * (index + 1));
        if index < 4 {
            assert!(
                matches!(outcome.expect("admission"), SubmitOutcome::Accepted),
                "texture {index}"
            );
            let deadline = Instant::now() + Duration::from_secs(2);
            while encoder.counters().retained_leases != 0 && Instant::now() < deadline {
                match encoder.try_packet() {
                    Ok(_) => std::thread::sleep(Duration::from_millis(1)),
                    Err(error) => panic!("unexpected error: {error}"),
                }
            }
        } else {
            outcome.expect("admission");
        }
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut observed = None;
    while observed.is_none() && Instant::now() < deadline {
        match encoder.try_packet() {
            Ok(_) => std::thread::sleep(Duration::from_millis(1)),
            Err(error) => observed = Some(error),
        }
    }
    let error = observed.expect("a fifth texture must be refused");
    assert!(
        error.message.contains("fifth capture texture"),
        "{}",
        error.message
    );
    assert_eq!(encoder.counters().registrations, 4);
    assert!(encoder.counters().identities().is_ok());
}

#[test]
fn a_registration_failure_is_explicit_and_does_not_leak_a_lease() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    lock(&state).script.register_unsupported = true;
    let mut encoder = open_encoder(&source, &state);
    assert!(matches!(
        submit(&mut encoder, &source, FRAME_PERIOD_NS).expect("admission"),
        SubmitOutcome::Accepted
    ));
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut observed = None;
    while observed.is_none() && Instant::now() < deadline {
        match encoder.try_packet() {
            Ok(_) => std::thread::sleep(Duration::from_millis(1)),
            Err(error) => observed = Some(error),
        }
    }
    let error = observed.expect("the registration failure must surface");
    assert_eq!(encoder.native_failure().unwrap().message, error.message);
    assert_eq!(error.code, ErrorCode::UnsupportedCodec);
    let counters = encoder.counters();
    assert_eq!(counters.registrations, 0);
    assert_eq!(counters.retained_leases, 0);
    assert_eq!(source.free_slots(), SLOT_COUNT);
    assert!(counters.identities().is_ok(), "{:?}", counters.identities());
}

#[test]
fn a_cleanup_failure_is_reported_while_every_resource_is_still_released() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    lock(&state).script.close_fails = true;
    let mut encoder = open_encoder(&source, &state);
    encoder.begin_drain().expect("drain");
    // The drain completes, but teardown reports the cleanup failure.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut observed = None;
    while observed.is_none() && Instant::now() < deadline {
        match encoder.try_packet() {
            Ok(_) => std::thread::sleep(Duration::from_millis(1)),
            Err(error) => observed = Some(error),
        }
    }
    let error = observed.expect("the cleanup failure must surface");
    assert_eq!(error.code, ErrorCode::Internal);
    assert_eq!(lock(&state).closes, 1);
    assert_eq!(encoder.counters().retained_leases, 0);
    assert!(encoder.counters().identities().is_ok());
}

#[test]
fn a_busy_driver_is_retried_instead_of_failing_or_dropping_the_input() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    lock(&state).script.busy_submits = 3;
    lock(&state).script.eos_busy = 2;
    let mut encoder = open_encoder(&source, &state);
    assert!(matches!(
        submit(&mut encoder, &source, FRAME_PERIOD_NS).expect("admission"),
        SubmitOutcome::Accepted
    ));
    encoder.begin_drain().expect("drain");
    let packets = collect_packets(&mut encoder, 1, Duration::from_secs(3));
    assert_eq!(packets.len(), 1, "the retried input must still be encoded");
    wait_drained(&mut encoder, Duration::from_secs(3)).expect("drain completes after retries");
    let counters = encoder.counters();
    assert_eq!(counters.native_busy_retries, 3);
    assert_eq!(counters.eos_busy_retries, 2);
    assert_eq!(counters.accepted_inputs, 1);
    assert_eq!(counters.native_submissions, 1);
    assert!(counters.identities().is_ok(), "{:?}", counters.identities());
    encoder.stop().expect("clean stop");
}

#[test]
fn a_strictly_increasing_source_timeline_is_enforced_before_admission() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    let mut encoder = open_encoder(&source, &state);
    assert!(matches!(
        submit(&mut encoder, &source, 1_000_000).expect("admission"),
        SubmitOutcome::Accepted
    ));
    let repeated = match submit(&mut encoder, &source, 1_000_000) {
        Err(error) => error,
        Ok(_) => panic!("a repeated timestamp must be refused"),
    };
    assert_eq!(repeated.code, ErrorCode::InvalidRequest);
    let backwards = match submit(&mut encoder, &source, 999_999) {
        Err(error) => error,
        Ok(_) => panic!("a backwards timestamp must be refused"),
    };
    assert_eq!(backwards.code, ErrorCode::InvalidRequest);
    // A duration that overflows the nanosecond timeline is refused as well.
    let overflow = match encoder.submit(EncoderInput {
        input: source.take(2_000_000),
        duration_ns: NonZeroU64::new(u64::MAX).expect("non-zero"),
    }) {
        Err(error) => error,
        Ok(_) => panic!("an overflowing timestamp+duration must be refused"),
    };
    assert_eq!(overflow.code, ErrorCode::InvalidRequest);
    // Live snapshots are not atomic across the worker's counter updates.
    encoder.begin_drain().unwrap();
    assert_eq!(
        collect_packets(&mut encoder, 1, Duration::from_secs(2)).len(),
        1
    );
    wait_drained(&mut encoder, Duration::from_secs(2)).unwrap();
    encoder.stop().unwrap();
    let counters = encoder.counters();
    assert_eq!(counters.accepted_inputs, 1);
    assert!(counters.identities().is_ok());
}

#[test]
fn an_unusable_configuration_is_refused_before_any_native_work() {
    let num = NonZeroU32::new(60).expect("non-zero");
    let den = NonZeroU32::new(1).expect("non-zero");
    // Odd dimensions cannot be 4:2:0.
    assert_eq!(
        NativeEncoderConfig::proof(65, 48, num, den)
            .expect_err("odd width")
            .code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        NativeEncoderConfig::proof(64, 0, num, den)
            .expect_err("zero height")
            .code,
        ErrorCode::InvalidRequest
    );
    assert!(NativeEncoderConfig::proof(64, 48, num, den).is_ok());
    // No invented nominal-rate ceiling: any representable positive rational is accepted
    // here and the driver decides whether it is supported.
    let very_fast = NonZeroU32::new(u32::MAX).expect("non-zero");
    assert!(NativeEncoderConfig::proof(64, 48, very_fast, den).is_ok());

    // Initialization bytes above the accepted bound are refused before the worker runs.
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    let mut oversized = settings();
    oversized.codec_config = vec![0u8; MAX_CODEC_CONFIG_BYTES + 1];
    let driver = ScriptedDriver::new(oversized, Arc::clone(&state));
    let error = match WindowsH264Encoder::with_driver(
        config(),
        source.identity,
        Box::new(driver),
        Arc::clone(&source.payload),
    ) {
        Err(error) => error,
        Ok(_) => panic!("initialization bytes above the bound must be refused"),
    };
    assert_eq!(error.code, ErrorCode::Internal);
    // The session was opened before that validation, so its owner thread must destroy it;
    // the refusal is reported first, so this waits for the owner's cleanup.
    let deadline = Instant::now() + Duration::from_secs(5);
    while lock(&state).closes == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        lock(&state).closes,
        1,
        "the refused session is still closed"
    );
}

#[test]
fn a_driver_that_applies_a_different_rate_or_quality_is_refused_before_admission() {
    for changed in ["rate", "quality"] {
        let source = TestSource::new(0x1000, false);
        let state = driver_state();
        let mut applied = settings();
        match changed {
            "rate" => applied.rate_den = 2,
            "quality" => applied.preset = NativeEncoderPreset::HighQuality,
            _ => unreachable!(),
        }
        let driver = ScriptedDriver::new(applied, Arc::clone(&state));
        let error = WindowsH264Encoder::with_driver(
            config(),
            source.identity,
            Box::new(driver),
            Arc::clone(&source.payload),
        )
        .expect_err("the driver must apply the frozen plan exactly");
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(
            error
                .message
                .contains("requested geometry, rate, bitrate, GOP or quality")
        );
        assert_eq!(lock(&state).closes, 1, "refused driver closes on its owner");
    }
}

#[test]
fn a_lease_without_gpu_backing_is_refused_instead_of_registered() {
    let lifetime = PayloadLifetime::new();
    let pool = SurfacePool::new(
        plan_owned_payload(WIDTH, HEIGHT, false)
            .expect("the synthetic payload fits")
            .slots_bytes,
    );
    // A payload owner that reports no GPU backing at all.
    struct Backingless;
    impl PayloadOwner for Backingless {
        fn encode_texture(&self, _slot_index: usize) -> Option<EncodeTexture> {
            None
        }
    }
    let payload: Arc<dyn PayloadOwner> = Arc::new(Backingless);
    let slot = pool.try_acquire_copy_slot().expect("a free slot");
    slot.mark_ready();
    slot.mark_consumer_owned();
    let shared_payload = Arc::clone(&payload);
    let surface = WindowsSurface::new(slot, payload);
    let state = driver_state();
    let driver = ScriptedDriver::new(settings(), Arc::clone(&state));
    let mut encoder = WindowsH264Encoder::with_driver(
        config(),
        SurfaceIdentity {
            pool: pool.id(),
            device: 0x1000,
            texture: 0x4000,
            width: WIDTH,
            height: HEIGHT,
        },
        Box::new(driver),
        shared_payload,
    )
    .expect("the scripted session opens");
    let frame = CapturedFrame {
        surface,
        timestamp: MediaTimestamp(FRAME_PERIOD_NS),
        width: WIDTH,
        height: HEIGHT,
    };
    assert!(matches!(
        encoder
            .submit(EncoderInput {
                input: frame,
                duration_ns: duration(),
            })
            .expect("admission"),
        SubmitOutcome::Accepted
    ));
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut observed = None;
    while observed.is_none() && Instant::now() < deadline {
        match encoder.try_packet() {
            Ok(_) => std::thread::sleep(Duration::from_millis(1)),
            Err(error) => observed = Some(error),
        }
    }
    let error = observed.expect("a backingless lease must be refused");
    assert_eq!(error.code, ErrorCode::Internal);
    assert_eq!(encoder.counters().rejected_inputs, 1);
    assert!(encoder.counters().identities().is_ok());
    let _ = lifetime;
}

#[test]
fn registration_backing_outlives_every_frame_lease_and_a_capture_stop() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    let mut encoder = open_encoder(&source, &state);
    assert!(matches!(
        submit(&mut encoder, &source, FRAME_PERIOD_NS).expect("admission"),
        SubmitOutcome::Accepted
    ));
    let packets = collect_packets(&mut encoder, 1, Duration::from_secs(3));
    assert_eq!(packets.len(), 1);
    let weak = source.weak.clone();
    let pool = Arc::clone(&source.pool);
    drop(source);
    assert_eq!(encoder.counters().retained_leases, 0);
    assert_eq!(pool.counts().free, SLOT_COUNT);
    assert_eq!(lock(&state).unregisters, 0);
    assert!(
        weak.upgrade().is_some(),
        "only the encoder now owns the backing"
    );
    encoder.begin_drain().expect("drain");
    wait_drained(&mut encoder, Duration::from_secs(3)).expect("drain");
    encoder.stop().expect("clean stop");
    assert!(
        weak.upgrade().is_none(),
        "joined worker releases its last backing owner"
    );
    assert_eq!(lock(&state).unregisters, 1);
    assert_eq!(lock(&state).closes, 1);
}

#[test]
fn a_failed_unregister_is_reported_and_never_counted_as_a_release() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    lock(&state).script.unregister_fails = true;
    let mut encoder = open_encoder(&source, &state);
    assert!(matches!(
        submit(&mut encoder, &source, FRAME_PERIOD_NS).expect("admission"),
        SubmitOutcome::Accepted
    ));
    encoder.begin_drain().expect("drain");
    // The packet is delivered and the same failure is published as the sticky terminal
    // error, because a cleanup that could not release a registration has to surface.
    let mut delivered = 0usize;
    let mut observed = None;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match encoder.try_packet() {
            Ok(EncoderPoll::Packet(_)) => delivered += 1,
            Ok(EncoderPoll::Drained) => break,
            Ok(EncoderPoll::Pending) => std::thread::sleep(Duration::from_millis(1)),
            Err(error) => {
                observed = Some(error);
                break;
            }
        }
    }
    // The failure takes precedence over a queued packet, so delivery before it is allowed
    // but not required; what must hold is that the failure surfaces and nothing is
    // silently lost.
    let _ = delivered;
    let error = observed.expect("a failed unregister must surface");
    assert_eq!(error.code, ErrorCode::Internal);
    let stop = encoder.stop();
    assert!(
        stop.is_err(),
        "a failed unregister must fail the finalization"
    );
    let counters = encoder.counters();
    assert_eq!(counters.registrations, 1);
    assert_eq!(
        counters.unregistrations, 0,
        "a failed unregister is not a released registration"
    );
    assert!(counters.identities().is_ok(), "{:?}", counters.identities());
}

#[test]
fn final_tail_backpressure_with_an_available_packet_still_reconciles() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    let mut encoder = open_encoder(&source, &state);
    for index in 0..MAX_ENCODER_LEASES {
        assert!(matches!(
            submit(&mut encoder, &source, FRAME_PERIOD_NS * (index as u64 + 1)).expect("admission"),
            SubmitOutcome::Accepted
        ));
    }
    // The final-tail admission is refused while every lease is retained, and the exact
    // owned input comes back.
    let tail = source.take(FRAME_PERIOD_NS * 100);
    let tail_pts = tail.timestamp;
    let refused = encoder
        .submit(EncoderInput {
            input: tail,
            duration_ns: duration(),
        })
        .expect("pressure is not an error");
    let mut pending = match refused {
        SubmitOutcome::Backpressure(input) => input,
        SubmitOutcome::Accepted => panic!("the tail frame must be refused under pressure"),
    };
    assert_eq!(pending.input.timestamp, tail_pts);
    // A packet becomes available, which frees the bound, and the same owned input is
    // accepted without any packet being dropped on the way.
    let packets = collect_packets(&mut encoder, 1, Duration::from_secs(3));
    assert_eq!(packets.len(), 1);
    let mut accepted = false;
    for _ in 0..2000 {
        match encoder.submit(EncoderInput {
            input: pending.input,
            duration_ns: pending.duration_ns,
        }) {
            Ok(SubmitOutcome::Accepted) => {
                accepted = true;
                break;
            }
            Ok(SubmitOutcome::Backpressure(input)) => {
                pending = input;
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("the retry must not fail: {error}"),
        }
    }
    assert!(
        accepted,
        "the retained input is accepted once a credit is free"
    );
    encoder.begin_drain().expect("drain");
    let delivered = collect_packets(&mut encoder, MAX_ENCODER_LEASES, Duration::from_secs(5));
    assert_eq!(delivered.len(), MAX_ENCODER_LEASES);
    wait_drained(&mut encoder, Duration::from_secs(3)).expect("drain");
    encoder.stop().expect("clean stop");
    let counters = encoder.counters();
    assert_eq!(counters.accepted_inputs, counters.packets_emitted);
    assert_eq!(counters.packets_dequeued, counters.packets_emitted);
    assert_eq!(counters.packets_discarded_on_stop, 0);
    assert!(counters.backpressure_returns >= 1);
    assert!(counters.identities().is_ok(), "{:?}", counters.identities());
}
#[test]
fn an_unconfirmed_cleanup_retains_ownership_and_never_publishes_drained() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    // Model two consecutive unresolved native closes before release is confirmed.
    lock(&state).script.cleanup_unsafe_attempts = 2;
    let mut encoder = open_encoder(&source, &state);
    assert!(matches!(
        submit(&mut encoder, &source, FRAME_PERIOD_NS).expect("admission"),
        SubmitOutcome::Accepted
    ));
    encoder.begin_drain().expect("drain");
    // The picture is delivered, but the drain itself must never be published as successful
    // because cleanup could not confirm that the driver finished with its resources.
    let mut delivered = 0usize;
    let mut observed = None;
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        match encoder.try_packet() {
            Ok(EncoderPoll::Packet(_)) => delivered += 1,
            Ok(EncoderPoll::Drained) => {
                panic!("Drained must not be published after unsafe cleanup")
            }
            Ok(EncoderPoll::Pending) => std::thread::sleep(Duration::from_millis(2)),
            Err(error) => {
                observed = Some(error);
                break;
            }
        }
    }
    // The failure takes precedence over the queued packet, so delivery before it is allowed
    // but not required: what must hold is that the failure surfaces and the accepted input
    // is accounted for rather than silently lost.
    let _ = delivered;
    assert_eq!(
        encoder.counters().packets_emitted,
        1,
        "the accepted input reached the driver and produced its access unit"
    );
    let error = observed.expect("an unconfirmed cleanup must surface as a failure");
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(
        encoder.stop().is_err(),
        "finalization must fail when native completion was never confirmed"
    );
    let counters = encoder.counters();
    // The unresolved close was retried on the owner worker with the handle still reachable,
    // and it succeeded on the third attempt: delayed recovery, then exactly-once join.
    assert!(
        counters.unsafe_cleanup_steps >= 2,
        "the unresolved close must be recorded before it is retried"
    );
    assert_eq!(
        lock(&state).closes,
        3,
        "two failed closes, then one successful close"
    );
    // After confirmed cleanup the previously retained slot is released exactly once.
    assert_eq!(source.free_slots(), SLOT_COUNT);
    // The picture had already been collected and its registration released before `close`
    // reported the failure, so no capture slot is retained in this case; the retained owner
    // holds the session and the DLL reference. Lease retention is covered by the test that
    // fails cleanup while a picture is still in flight.
    assert!(counters.identities().is_ok(), "{:?}", counters.identities());
}
#[test]
fn diagnostics_report_the_bounded_state_and_the_checked_identities() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    let mut encoder = open_encoder(&source, &state);
    assert!(matches!(
        submit(&mut encoder, &source, FRAME_PERIOD_NS).expect("admission"),
        SubmitOutcome::Accepted
    ));
    encoder.begin_drain().expect("drain");
    let packets = collect_packets(&mut encoder, 1, Duration::from_secs(2));
    assert_eq!(packets.len(), 1);
    wait_drained(&mut encoder, Duration::from_secs(2)).expect("drain");
    let report = encoder.diagnostics();
    assert!(report.contains("FRAME-0104"), "{report}");
    assert!(report.contains("64x48 at 60/1"), "{report}");
    assert!(
        report.contains("accounting: every checked identity holds"),
        "{report}"
    );
    assert!(report.contains("terminal_error: none"), "{report}");
    assert_eq!(
        encoder.info().codec_config.len(),
        settings().codec_config.len()
    );
    assert!(!encoder.info().allows_reordering);
    assert_eq!(encoder.settings().width, WIDTH);
    encoder.stop().expect("clean stop");
    assert!(encoder.diagnostics().contains("state: stopped"));
}

#[test]
fn the_packet_queue_never_grows_past_its_bound_and_accounts_every_packet() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    let mut encoder = open_encoder(&source, &state);
    for index in 0..3u64 {
        assert!(matches!(
            submit(&mut encoder, &source, FRAME_PERIOD_NS * (index + 1)).expect("admission"),
            SubmitOutcome::Accepted
        ));
    }
    encoder.begin_drain().expect("drain");
    // Do not consume anything: the worker still completes and queues bounded output.
    let deadline = Instant::now() + Duration::from_secs(2);
    while encoder.counters().packets_held < 3 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    let counters = encoder.counters();
    assert!(counters.packets_held <= MAX_PACKETS as u64);
    assert!(counters.bytes_held <= MAX_PACKET_BYTES);
    assert!(counters.identities().is_ok(), "{:?}", counters.identities());
    let packets = collect_packets(&mut encoder, 3, Duration::from_secs(2));
    assert_eq!(packets.len(), 3);
    wait_drained(&mut encoder, Duration::from_secs(2)).expect("drain");
    encoder.stop().expect("clean stop");
    let counters = encoder.counters();
    assert_eq!(counters.packets_dequeued, 3);
    assert_eq!(counters.packets_held, 0);
    assert!(counters.identities().is_ok(), "{:?}", counters.identities());
    let _ = c_void_unused();
}

/// Keeps the `c_void` import meaningful for the pointer construction above.
fn c_void_unused() -> Option<*mut c_void> {
    None
}

#[test]
fn stop_and_drop_wait_for_real_cleanup_without_releasing_backing_or_slots() {
    for drop_facade in [false, true] {
        let source = TestSource::new(0x1000, false);
        let state = driver_state();
        let gate = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let mut driver = lock(&state);
            driver.script.never_complete = true;
            driver.script.cleanup_gate = Some(Arc::clone(&gate));
        }
        let mut encoder = open_encoder(&source, &state);
        submit(&mut encoder, &source, FRAME_PERIOD_NS).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while lock(&state).submissions.is_empty() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(lock(&state).submissions.len(), 1);
        let weak = source.weak.clone();
        let pool = Arc::clone(&source.pool);
        let shared = Arc::clone(&encoder.shared);
        drop(source);
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            if drop_facade {
                drop(encoder);
            } else {
                assert!(encoder.stop().is_err());
            }
            tx.send(()).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while lock(&state).closes == 0 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        // Release the gate before asserting, so a failing assertion never strands a worker.
        let observed = (
            lock(&state).closes,
            pool.counts().free,
            weak.upgrade().is_some(),
            shared.worker_finished.load(Ordering::Acquire),
            rx.try_recv().is_ok(),
        );
        gate.store(true, Ordering::Release);
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        thread.join().unwrap();
        assert!(observed.0 >= 1);
        assert_eq!(observed.1, SLOT_COUNT - 1);
        assert!(observed.2 && !observed.3 && !observed.4);
        assert_eq!(pool.counts().free, SLOT_COUNT);
        assert!(weak.upgrade().is_none());
        assert!(lock(&state).closes >= 2);
    }
}

#[test]
fn scratch_and_simultaneous_packet_reservations_share_the_byte_limit() {
    let unit = u64::from(MAX_ACCESS_UNIT_BYTES);
    assert!(held_credits_available(0, 0, 2, 2 * unit, unit));
    assert!(held_credits_available(0, 0, 3, 3 * unit, unit));
    assert!(!held_credits_available(0, 0, 4, 4 * unit, unit));
    assert!(held_credits_available(3, 3 * unit, 0, 0, unit));
    assert!(!held_credits_available(3, 4 * unit, 0, 0, unit));
    assert!(held_credits_available(2, 2 * unit, 0, 0, unit));
}

#[test]
fn failed_preparation_keeps_backing_until_delayed_cleanup_finishes() {
    let source = TestSource::new(0x1000, false);
    let weak = source.weak.clone();
    let state = driver_state();
    let gate = Arc::new(std::sync::atomic::AtomicBool::new(false));
    lock(&state).script.cleanup_gate = Some(Arc::clone(&gate));
    let identity = source.identity;
    let backing = Arc::clone(&source.payload);
    let driver_state = Arc::clone(&state);
    drop(source);
    let thread = std::thread::spawn(move || {
        WindowsH264Encoder::start_with(config(), identity, backing, move || {
            let mut invalid = settings();
            invalid.codec_config.clear();
            Ok(Box::new(ScriptedDriver::new(invalid, driver_state)))
        })
        .is_err()
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while lock(&state).closes == 0 && Instant::now() < deadline {
        std::thread::yield_now();
    }
    let observed = (
        weak.upgrade().is_some(),
        thread.is_finished(),
        lock(&state).closes,
    );
    gate.store(true, Ordering::Release);
    assert!(thread.join().unwrap());
    assert!(observed.0 && !observed.1 && observed.2 > 0);
    assert!(weak.upgrade().is_none());
}

#[test]
fn cleanup_context_preserves_both_failures() {
    let error = super::combine_cleanup_errors(
        Some(internal_error("primary unregister".into())),
        Some(internal_error("secondary destroy".into())),
    );
    assert!(
        error.message.contains("primary unregister") && error.message.contains("secondary destroy")
    );
}

#[test]
fn full_packet_credits_return_the_same_input_before_capture_exhaustion() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    let mut encoder = open_encoder(&source, &state);
    for i in 1..=super::MAX_PACKETS {
        let mut input = EncoderInput {
            input: source.take(FRAME_PERIOD_NS * i as u64),
            duration_ns: duration(),
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            input = match encoder.submit(input).unwrap() {
                SubmitOutcome::Accepted => break,
                SubmitOutcome::Backpressure(input) => input,
            };
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        while encoder.counters().packets_emitted < i as u64 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(encoder.counters().packets_emitted, i as u64);
    }
    assert_eq!(encoder.counters().packets_held, super::MAX_PACKETS as u64);
    assert_eq!(source.free_slots(), SLOT_COUNT);
    let input = EncoderInput {
        input: source.take(FRAME_PERIOD_NS * 10),
        duration_ns: duration(),
    };
    let identity = input.input.surface.encode_texture().unwrap().identity;
    let pending = match encoder.submit(input).unwrap() {
        SubmitOutcome::Backpressure(input) => input,
        _ => panic!("full credits must refuse"),
    };
    assert_eq!(
        pending.input.surface.encode_texture().unwrap().identity,
        identity
    );
    assert_eq!(pending.input.timestamp.0, FRAME_PERIOD_NS * 10);
    assert_eq!(
        collect_packets(&mut encoder, super::MAX_PACKETS, Duration::from_secs(2)).len(),
        super::MAX_PACKETS
    );
    assert!(matches!(
        encoder.submit(pending).unwrap(),
        SubmitOutcome::Accepted
    ));
    encoder.begin_drain().unwrap();
    assert_eq!(
        collect_packets(&mut encoder, 1, Duration::from_secs(2)).len(),
        1
    );
    wait_drained(&mut encoder, Duration::from_secs(2)).unwrap();
    encoder.stop().unwrap();
    assert!(encoder.counters().identities().is_ok());
}

#[test]
fn preparation_timeout_joins_the_late_owner_and_panic_cannot_detach_it() {
    for panic_factory in [false, true] {
        let source = TestSource::new(0x1000, false);
        let weak = source.weak.clone();
        let identity = source.identity;
        let backing = Arc::clone(&source.payload);
        let state = driver_state();
        let factory_state = Arc::clone(&state);
        drop(source);
        let (release, wait) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            WindowsH264Encoder::start_with(config(), identity, backing, move || {
                wait.recv().unwrap();
                assert!(!panic_factory, "scripted preparation panic");
                Ok(Box::new(ScriptedDriver::new(settings(), factory_state)))
            })
            .is_err()
        });
        if !panic_factory {
            std::thread::sleep(super::PREPARATION_WATCHDOG + Duration::from_millis(100));
            let blocked = !thread.is_finished() && weak.upgrade().is_some();
            release.send(()).unwrap();
            assert!(thread.join().unwrap());
            assert!(
                blocked,
                "the readiness deadline must not detach its late worker"
            );
            assert_eq!(lock(&state).closes, 1);
        } else {
            release.send(()).unwrap();
            assert!(thread.join().unwrap());
        }
        assert!(weak.upgrade().is_none());
    }
}

#[test]
fn drained_never_precedes_a_pending_unregister_that_later_fails() {
    let source = TestSource::new(0x1000, false);
    let state = driver_state();
    let gate = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let mut state = lock(&state);
        state.script.unregister_gate = Some(Arc::clone(&gate));
        state.script.unregister_fails = true;
    }
    let mut encoder = open_encoder(&source, &state);
    submit(&mut encoder, &source, FRAME_PERIOD_NS).unwrap();
    encoder.begin_drain().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !lock(&state).script.unregister_entered && Instant::now() < deadline {
        std::thread::yield_now();
    }
    let entered = lock(&state).script.unregister_entered;
    let mut optimistic = false;
    for _ in 0..100 {
        optimistic |= matches!(encoder.try_packet(), Ok(EncoderPoll::Drained));
    }
    gate.store(true, Ordering::Release);
    assert!(entered && !optimistic);
    assert!(encoder.stop().is_err());
    assert!(encoder.try_packet().is_err());
}
