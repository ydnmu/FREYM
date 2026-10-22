//! Real Windows hardware encoder tests (FRAME-0104, ADR 0006).
//!
//! These tests drive the production capture session and the production NVENC/D3D11
//! encoder on the local display. They assert platform-independent invariants, never a
//! specific adapter, driver or measured rate, and they are the only encoder tests that
//! need hardware.
//!
//! A missing display, a missing or too-old NVIDIA driver, or a session that cannot be
//! created is reported as an explicit **skip** with its reason. Any other failure — a
//! native error, a broken accounting identity, a missing access unit or a changed
//! timestamp — fails the test, because a defect must never disappear behind a skip.
//!
//! A skipped run is not hardware evidence and is never recorded as a pass.
#![cfg(target_os = "windows")]

use std::num::{NonZeroU32, NonZeroU64};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use frame_capture::{
    CaptureBackend, CaptureCancellation, CaptureContext, CaptureSession, CapturedFrame,
    DisplayBackend,
};
use frame_clock::MasterClock;
use frame_contracts::{CaptureTarget, DisplayId, ErrorCode, FpsMode, RecorderError};
use frame_encode::{EncoderInput, EncoderPoll, PacketEncoder, SubmitOutcome};
use frame_windows::{
    NativeEncoderConfig, PlatformBackend, WindowsCaptureSession, WindowsH264Encoder, WindowsSurface,
};

/// Every test in this file takes this lock for its whole duration.
///
/// Windows allows one desktop duplication per output, and the measured native behaviour
/// of this device is that a hardware encoder session running while duplication or its
/// copies use the same adapter can stop making progress (STATUS.md, FRAME-0104). Running
/// these tests concurrently would therefore produce spurious failures and hangs, so they
/// take turns inside this binary; the tests themselves remain independent processes of
/// the same production API.
static HARDWARE_LOCK: Mutex<()> = Mutex::new(());

struct HardwareGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    stop: std::sync::mpsc::Sender<()>,
    sampler: Option<std::thread::JoinHandle<()>>,
}
impl Drop for HardwareGuard {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.sampler.take() {
            let _ = thread.join();
        }
    }
}
fn lock_hardware() -> HardwareGuard {
    let lock = HARDWARE_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let (stop, receiver) = std::sync::mpsc::channel();
    let sampler = std::thread::spawn(move || {
        while receiver
            .recv_timeout(Duration::from_secs(2))
            .is_err_and(|error| matches!(error, std::sync::mpsc::RecvTimeoutError::Timeout))
        {
            eprintln!(
                "hardware test witness: {}",
                WindowsH264Encoder::native_call_witness()
            );
        }
    });
    HardwareGuard {
        _lock: lock,
        stop,
        sampler: Some(sampler),
    }
}

/// The first attached display, or a reported skip.
fn display_or_skip() -> Option<frame_contracts::DisplayInfo> {
    let displays = PlatformBackend
        .enumerate_displays()
        .expect("display enumeration must not fail on a Windows desktop");
    match displays.first() {
        Some(display) => Some(display.clone()),
        None => {
            eprintln!("SKIP (not hardware evidence): this Windows session reports no display.");
            None
        }
    }
}

/// True for the documented conditions that make a native encoder or capture session
/// unavailable in this session; everything else is a defect and must fail the test.
fn unavailable(error: &RecorderError) -> bool {
    matches!(
        error.code,
        ErrorCode::EncoderUnavailable | ErrorCode::NotImplemented | ErrorCode::PermissionDenied
    )
}

fn start_capture(
    display: &frame_contracts::DisplayInfo,
) -> Result<WindowsCaptureSession, RecorderError> {
    let mut backend = PlatformBackend;
    backend.start(
        &CaptureTarget::Display(DisplayId(display.id.0.clone())),
        FpsMode::AutoMatchSource,
        CaptureContext {
            clock: std::sync::Arc::new(MasterClock::default()),
            cancellation: CaptureCancellation::default(),
        },
    )
}

/// Takes one completed frame, or `None` when the bounded wait elapsed.
fn take_frame(
    session: &mut WindowsCaptureSession,
    timeout: Duration,
) -> Result<Option<CapturedFrame<WindowsSurface>>, RecorderError> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(frame) = session.try_frame()? {
            return Ok(Some(frame));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Collects every ready packet, with a bounded wait for the next one.
fn collect_packets(
    encoder: &mut WindowsH264Encoder,
    wanted: usize,
    timeout: Duration,
) -> Result<Vec<frame_encode::EncodedPacket>, RecorderError> {
    let mut packets = Vec::new();
    let deadline = Instant::now() + timeout;
    while packets.len() < wanted && Instant::now() < deadline {
        match encoder.try_packet()? {
            EncoderPoll::Packet(packet) => packets.push(packet),
            EncoderPoll::Pending => std::thread::sleep(Duration::from_millis(1)),
            EncoderPoll::Drained => break,
        }
    }
    Ok(packets)
}

#[test]
fn native_qualification_reports_only_verified_hardware_encoders() {
    let _hardware_guard = lock_hardware();
    let diagnostics = frame_windows::encode_diagnostics()
        .expect("encoder diagnostics must not fail on a Windows desktop");
    assert!(diagnostics.contains("FRAME-0104"), "{diagnostics}");

    match frame_windows::probe_native_encoders() {
        Ok(candidates) => {
            for candidate in &candidates {
                assert!(
                    candidate.capability.hardware,
                    "only hardware candidates may appear"
                );
                assert_eq!(candidate.capability.codec, "h264");
                assert!(!candidate.adapter.is_empty());
                // The qualification configuration is the explicit ADR 0006 one.
                assert_eq!(candidate.settings.width, frame_windows::QUALIFICATION_WIDTH);
                assert_eq!(
                    candidate.settings.height,
                    frame_windows::QUALIFICATION_HEIGHT
                );
                assert_eq!(
                    candidate.settings.rate_num,
                    frame_windows::QUALIFICATION_RATE_NUM
                );
                assert_eq!(
                    candidate.settings.rate_den,
                    frame_windows::QUALIFICATION_RATE_DEN
                );
                // Every capability answer came from the driver, not from a vendor name.
                assert!(
                    candidate.settings.enable_async,
                    "asynchronous events are required"
                );
                assert!(
                    candidate.settings.caps_input_argb,
                    "ARGB input must be verified"
                );
                assert!(candidate.settings.caps_h264_guid);
                assert!(candidate.settings.caps_profile_high);
                assert!(candidate.settings.caps_preset_available);
                // The qualification request's own rate control and preset are reported
                // back, so a plan that asks for another pair can be checked against the
                // driver's answer instead of the request.
                assert_eq!(
                    candidate.settings.rate_control,
                    frame_windows::NativeRateControl::Cbr
                );
                assert_eq!(
                    candidate.settings.preset,
                    frame_windows::NativeEncoderPreset::LowLatency
                );
                assert_eq!(
                    candidate.settings.max_bitrate_bps,
                    candidate.settings.bitrate_bps
                );
                assert!(candidate.settings.rate_control_support().cbr);
                assert!(candidate.settings.caps_width_max >= candidate.settings.width);
                assert!(candidate.settings.caps_height_max >= candidate.settings.height);
                assert_eq!(candidate.settings.frame_interval_p, 1);
                assert!(candidate.settings.enable_ptd);
                assert!(candidate.settings.repeat_spspps);
                assert!(candidate.settings.output_buffers <= 4);
                assert!(!candidate.settings.codec_config.is_empty());
                assert!(
                    candidate.settings.codec_config.len() <= frame_windows::MAX_CODEC_CONFIG_BYTES
                );
            }
            if candidates.is_empty() {
                eprintln!(
                    "SKIP (not hardware evidence): no NVIDIA encoder adapter qualified in this session."
                );
            }
        }
        Err(error) if unavailable(&error) => {
            eprintln!("SKIP (not hardware evidence): {}.", error.message);
        }
        Err(error) => panic!("qualification must not fail unexpectedly: {error}"),
    }
}

#[test]
fn real_captured_surfaces_encode_into_a_complete_annex_b_stream() {
    let _hardware_guard = lock_hardware();
    let Some(display) = display_or_skip() else {
        return;
    };
    let mut session = match start_capture(&display) {
        Ok(session) => session,
        Err(error) if unavailable(&error) => {
            eprintln!(
                "SKIP (not hardware evidence): capture unavailable: {}.",
                error.message
            );
            return;
        }
        Err(error) => panic!("capture must not fail unexpectedly: {error}"),
    };

    let Some(first) = take_frame(&mut session, Duration::from_secs(5)).expect("the first frame")
    else {
        eprintln!("SKIP (not hardware evidence): the display produced no completed surface.");
        let _ = session.stop();
        return;
    };
    let config = NativeEncoderConfig::proof(
        first.width,
        first.height,
        NonZeroU32::new(60).expect("non-zero"),
        NonZeroU32::new(1).expect("non-zero"),
    )
    .expect("the captured geometry is a valid configuration");
    let mut encoder = match WindowsH264Encoder::open(config, &first.surface) {
        Ok(encoder) => encoder,
        Err(error) if unavailable(&error) => {
            eprintln!(
                "SKIP (not hardware evidence): encoder unavailable: {}.",
                error.message
            );
            let _ = session.stop();
            return;
        }
        Err(error) => panic!("the encoder must not fail unexpectedly: {error}"),
    };
    assert_eq!(encoder.settings().width, first.width);
    assert_eq!(encoder.settings().rate_num, 60);
    assert!(!encoder.info().codec_config.is_empty());

    // Submit a few real frames, deriving each duration from the next source timestamp
    // and giving the final one the explicit nominal period.
    let wanted = 5usize;
    let mut lookahead = Some(first);
    let mut pending: Option<EncoderInput<CapturedFrame<WindowsSurface>>> = None;
    let mut accepted = 0usize;
    let mut packets = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    while accepted < wanted && Instant::now() < deadline {
        if let Some(input) = pending.take() {
            match encoder.submit(input).expect("submission must not fail") {
                SubmitOutcome::Accepted => accepted += 1,
                SubmitOutcome::Backpressure(input) => {
                    pending = Some(input);
                    packets.extend(
                        collect_packets(&mut encoder, 1, Duration::from_millis(5))
                            .expect("packets stay readable under pressure"),
                    );
                    continue;
                }
            }
        }
        match take_frame(&mut session, Duration::from_secs(10)).expect("a source frame") {
            Some(frame) => {
                if let Some(previous) = lookahead.take() {
                    let duration =
                        frame_encode::duration_until(previous.timestamp, frame.timestamp)
                            .expect("source timestamps must be strictly increasing");
                    pending = Some(EncoderInput {
                        input: previous,
                        duration_ns: duration,
                    });
                }
                lookahead = Some(frame);
            }
            None => break,
        }
    }
    if let Some(input) = pending.take() {
        let mut input = Some(input);
        while let Some(current) = input.take() {
            match encoder.submit(current).expect("submission must not fail") {
                SubmitOutcome::Accepted => accepted += 1,
                SubmitOutcome::Backpressure(current) => {
                    input = Some(current);
                    packets.extend(
                        collect_packets(&mut encoder, 1, Duration::from_millis(5))
                            .expect("tail pressure must consume output"),
                    );
                    assert!(
                        Instant::now() < deadline + Duration::from_secs(10),
                        "tail admission must progress"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }
    if let Some(last) = lookahead.take() {
        let mut input = Some(EncoderInput {
            input: last,
            duration_ns: frame_encode::nominal_duration(
                NonZeroU32::new(60).expect("non-zero"),
                NonZeroU32::new(1).expect("non-zero"),
            ),
        });
        while let Some(current) = input.take() {
            match encoder.submit(current).expect("submission must not fail") {
                SubmitOutcome::Accepted => accepted += 1,
                SubmitOutcome::Backpressure(current) => {
                    input = Some(current);
                    packets.extend(
                        collect_packets(&mut encoder, 1, Duration::from_millis(5))
                            .expect("tail pressure must consume output"),
                    );
                    assert!(
                        Instant::now() < deadline + Duration::from_secs(10),
                        "tail admission must progress"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }
    assert!(accepted > 0, "at least one real frame must be accepted");

    encoder.begin_drain().expect("drain must be out of band");
    packets.extend(
        collect_packets(
            &mut encoder,
            accepted - packets.len(),
            Duration::from_secs(15),
        )
        .expect("packets must be delivered"),
    );
    assert_eq!(
        packets.len(),
        accepted,
        "every accepted input must be delivered"
    );
    for (index, packet) in packets.iter().enumerate() {
        assert!(!packet.data.is_empty(), "packet {index} is empty");
        assert!(
            packet.data.len() <= frame_windows::MAX_ACCESS_UNIT_BYTES as usize,
            "packet {index} exceeds the access-unit bound"
        );
        assert_eq!(packet.dts, packet.pts, "no reordering: DTS must equal PTS");
        assert!(NonZeroU64::new(packet.duration_ns).is_some());
        // GOP/IDR 120: the native IDR flag is expected at index 0 and at every 120th
        // access unit, and nowhere else.
        assert_eq!(
            packet.keyframe,
            index % 120 == 0,
            "the native IDR flag at index {index} does not follow the accepted GOP of 120"
        );
        if index > 0 {
            assert!(
                packet.pts.0 > packets[index - 1].pts.0,
                "timestamps must stay strictly increasing"
            );
        }
    }
    // `Drained` needs the last lease released as well as an empty queue, and that release
    // happens on the encoder worker, so this waits for the terminal state instead of
    // assuming it is already visible the instant the last packet was collected.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match encoder.try_packet().expect("a drained session") {
            EncoderPoll::Drained => break,
            EncoderPoll::Packet(_) => {
                panic!("a packet arrived after every accepted input was delivered")
            }
            EncoderPoll::Pending => assert!(
                Instant::now() < deadline,
                "a completed drain must report Drained"
            ),
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(encoder.drained());

    // The encoder is drained and stopped before the capture session here, which is the
    // order the proof example also uses: the measured native behaviour of this device is
    // that stopping capture while NVENC is still encoding on the same D3D11 device can
    // stall the capture worker (STATUS.md, FRAME-0104). The opposite order is covered
    // deliberately by `held_leases_survive_capture_stop_until_native_completion`.
    encoder.stop().expect("a completed drain must stop cleanly");
    session.stop().expect("capture must stop cleanly");
    let counters = encoder.counters();
    assert_eq!(
        counters.retained_leases, 0,
        "no capture lease may be retained"
    );
    assert_eq!(counters.registrations, counters.unregistrations);
    assert_eq!(counters.packets_held, 0);
    assert_eq!(counters.eos_submissions, 1);
    assert_eq!(counters.eos_acknowledgements, 1);
    assert_eq!(counters.timestamp_mismatches, 0);
    assert_eq!(counters.packets_discarded_on_stop, 0);
    assert!(counters.identities().is_ok(), "{:?}", counters.identities());
    assert!(
        encoder
            .diagnostics()
            .contains("accounting: every checked identity holds")
    );
}

/// Keeps the capture stop before the encoder stop, which ADR 0006 requires to be verified.
///
/// It runs in the default suite, under an external watchdog when it is invoked by the
/// verification runs: on this driver that ordering can stall a device call, and a hang is
/// recorded as a failure rather than hidden behind an ignored test.
#[test]
fn held_leases_survive_capture_stop_until_native_completion() {
    let _hardware_guard = lock_hardware();
    let Some(display) = display_or_skip() else {
        return;
    };
    let mut session = match start_capture(&display) {
        Ok(session) => session,
        Err(error) if unavailable(&error) => {
            eprintln!(
                "SKIP (not hardware evidence): capture unavailable: {}.",
                error.message
            );
            return;
        }
        Err(error) => panic!("capture must not fail unexpectedly: {error}"),
    };
    let Some(first) = take_frame(&mut session, Duration::from_secs(5)).expect("the first frame")
    else {
        eprintln!("SKIP (not hardware evidence): the display produced no completed surface.");
        let _ = session.stop();
        return;
    };
    let config = NativeEncoderConfig::proof(
        first.width,
        first.height,
        NonZeroU32::new(60).expect("non-zero"),
        NonZeroU32::new(1).expect("non-zero"),
    )
    .expect("the captured geometry is a valid configuration");
    let mut encoder = match WindowsH264Encoder::open(config, &first.surface) {
        Ok(encoder) => encoder,
        Err(error) if unavailable(&error) => {
            eprintln!(
                "SKIP (not hardware evidence): encoder unavailable: {}.",
                error.message
            );
            let _ = session.stop();
            return;
        }
        Err(error) => panic!("the encoder must not fail unexpectedly: {error}"),
    };

    // Three frames stay owned by the encoder while the capture session stops.
    let period = frame_encode::nominal_duration(
        NonZeroU32::new(60).expect("non-zero"),
        NonZeroU32::new(1).expect("non-zero"),
    );
    let mut frames = vec![first];
    // A static desktop can deliver updates very slowly, so this waits generously instead
    // of treating the source's own pacing as a failure.
    let mut waited = Duration::from_secs(0);
    while frames.len() < 2 && waited < Duration::from_secs(30) {
        match take_frame(&mut session, Duration::from_secs(5)).expect("a source frame") {
            Some(frame) => frames.push(frame),
            None => waited += Duration::from_secs(5),
        }
    }
    if frames.len() < 2 {
        eprintln!(
            "SKIP (not hardware evidence): the desktop delivered only {} completed surface(s); this test needs changing content.",
            frames.len()
        );
        let _ = session.stop();
        let _ = encoder.stop();
        return;
    }
    let submitted = frames.len();
    for frame in frames {
        let mut input = Some(EncoderInput {
            input: frame,
            duration_ns: period,
        });
        while let Some(current) = input.take() {
            match encoder.submit(current).expect("submission must not fail") {
                SubmitOutcome::Accepted => {}
                SubmitOutcome::Backpressure(current) => {
                    input = Some(current);
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }
    // Capture stops first: the leases the encoder still holds must remain valid.
    session.stop().expect("capture must stop cleanly");
    encoder.begin_drain().expect("drain must be out of band");
    let packets = collect_packets(&mut encoder, submitted, Duration::from_secs(15))
        .expect("the retained leases must still encode");
    assert_eq!(packets.len(), submitted);
    // The last lease is released on the encoder worker after its packet is queued, so the
    // terminal drain state can lag the final collection by one worker step.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match encoder.try_packet() {
            Ok(EncoderPoll::Drained) => break,
            Ok(EncoderPoll::Packet(_)) => panic!("a packet arrived after all were delivered"),
            Ok(EncoderPoll::Pending) => assert!(
                Instant::now() < deadline,
                "the drain must complete once every retained lease has been encoded"
            ),
            Err(error) => panic!("the encoder failed while draining retained leases: {error}"),
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        encoder.counters().retained_leases,
        0,
        "every retained lease must be released once the drain completed"
    );
    encoder
        .stop()
        .expect("the encoder must stop cleanly after capture stopped");
    let counters = encoder.counters();
    assert_eq!(counters.retained_leases, 0);
    assert!(counters.identities().is_ok(), "{:?}", counters.identities());
}
