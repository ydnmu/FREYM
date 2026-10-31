//! Synthetic unit tests for the capture worker, session, slot pool and timestamp
//! policy (FRAME-0103, corrected by FRAME-0103-R1).
//!
//! These tests drive the production loop through the private [`CaptureSource`] seam
//! with a visibly synthetic source, so they need no GPU and are not hardware
//! evidence. Timestamp conversion uses the canonical `frame_clock::ClockAnchor`, not
//! a second implementation.
//!
//! FRAME-0103-R1 added the five reproduced regressions: payload destruction after a
//! no-lease stop, a verification failure that must not orphan a slot, a primary error
//! that must survive a release failure, a post-readiness panic that must be sticky,
//! and a held diagnostic lock that must not block `try_frame`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use frame_capture::{CaptureCancellation, CaptureContext, CaptureSession};
use frame_clock::MasterClock;
use frame_contracts::{ErrorCode, FpsMode, RecorderError};

use super::diagnostics::NativeDescription;
use super::worker::{
    Acquired, CaptureSource, FrameObservation, ReadbackOutcome, ReadbackSample, WorkerContext,
    internal_error, source_lost_error,
};
use super::{
    CaptureOptions, CapturePreparation, SharedState, capture_probe, start_with_factory,
    validate_request,
};
use crate::display::CaptureOutput;
use crate::surface::{
    EncodeTexture, PayloadLifetime, PayloadOwner, SLOT_COUNT, SharedPayload, SlotState,
};

/// Synthetic selection: deliberately not a real display path.
fn synthetic_selection() -> CaptureOutput {
    CaptureOutput {
        device_path: "\\\\?\\DISPLAY#SYNTHETIC#1&0&UID0#{synthetic}".to_string(),
        adapter_name: "\\\\.\\SYNTHETIC1".to_string(),
        width: 64,
        height: 48,
        is_primary: true,
        refresh_rate_hz: Some(60.0),
        desktop_rect: (0, 0, 64, 48),
        monitor_handle: 0x5A17,
    }
}

/// A synthetic payload owner that reports its own destruction, so liveness is proved
/// by an observable owner counter instead of by slot labels.
struct SyntheticPayload {
    lifetime: Arc<PayloadLifetime>,
}

impl SyntheticPayload {
    fn new(lifetime: &Arc<PayloadLifetime>) -> Arc<Self> {
        lifetime.owner_created();
        Arc::new(Self {
            lifetime: Arc::clone(lifetime),
        })
    }
}

impl PayloadOwner for SyntheticPayload {
    fn encode_texture(&self, _slot_index: usize) -> Option<EncodeTexture> {
        // This synthetic capture source has no GPU backing, so an encoder must refuse it
        // rather than register a texture that does not exist.
        None
    }
}

impl Drop for SyntheticPayload {
    fn drop(&mut self) {
        self.lifetime.owner_dropped();
    }
}

/// One scripted acquisition outcome.
#[derive(Clone, Copy, Debug)]
enum Step {
    ReplaySample(i64),
    /// A desktop image with this native presentation timestamp.
    Image {
        last_present: i64,
        accumulated: u32,
    },
    /// One DXGI acquisition with both a desktop and a pointer update.
    ImageAndPointer {
        last_present: i64,
        last_mouse: i64,
    },
    /// A pointer-only update with a native QPC timestamp and unchanged desktop.
    PointerOnly(i64),
    /// A cursor change detected on timeout and timestamped at poll time.
    SyntheticCursor(i64),
    /// `DXGI_ERROR_WAIT_TIMEOUT`.
    Timeout,
    /// A terminal acquisition failure.
    Failure,
    /// A panic thrown from the worker thread after readiness.
    Panic,
    /// Waits before the next step, so a test can order the control thread's readiness
    /// handshake ahead of a worker-side cancellation without an unbounded wait.
    Delay(u64),
}

/// Scripted per-call behaviour.
#[derive(Clone, Copy, Default)]
struct ScriptPolicy {
    fail_activation: bool,
    require_nonblocking_after_copy: bool,
    query_complete: bool,
    fail_release: bool,
    fail_begin_copy: bool,
    panic_in_begin_copy: bool,
    fail_readback: bool,
    /// The verification reports cancellation (a normal stop) instead of a result.
    cancel_readback: bool,
    /// The source panics inside verification, after a slot has been claimed.
    panic_in_readback: bool,
}

/// A synthetic native source. Every field is scripted; there is no GPU here.
struct ScriptedSource {
    steps: VecDeque<Step>,
    policy: ScriptPolicy,
    copied: Vec<usize>,
    ticks_per_second: u64,
    counter: i64,
    outstanding: bool,
    /// Set when the script is exhausted, so a test can stop the loop deterministically.
    exhausted: Arc<AtomicBool>,
    payload: Arc<SyntheticPayload>,
    /// The session's token, so a cancellation path can request the stop it models.
    cancellation: CaptureCancellation,
    /// Optional handshake for tests that cancel after verification has started.
    readback_gate: Option<(
        std::sync::mpsc::SyncSender<()>,
        std::sync::mpsc::Receiver<()>,
    )>,
    description: NativeDescription,
}

impl ScriptedSource {
    fn new(
        steps: Vec<Step>,
        policy: ScriptPolicy,
        exhausted: Arc<AtomicBool>,
        lifetime: &Arc<PayloadLifetime>,
        cancellation: CaptureCancellation,
    ) -> Self {
        Self {
            steps: steps.into(),
            policy,
            copied: Vec::new(),
            // The synthetic counter advances 0.1 s per image at 10 MHz.
            ticks_per_second: 10_000_000,
            counter: 0,
            outstanding: false,
            exhausted,
            payload: SyntheticPayload::new(lifetime),
            cancellation,
            readback_gate: None,
            description: NativeDescription {
                api: "SYNTHETIC (not hardware)".to_string(),
                adapter_description: "Synthetic Adapter".to_string(),
                output_gdi_name: "\\\\.\\SYNTHETIC1".to_string(),
                width: 64,
                height: 48,
                pixel_format: "SYNTHETIC_BGRA8".to_string(),
                rotation_identity: true,
                desktop_image_in_system_memory: false,
                context_multithread_protected: true,
                staging_bytes: 0,
            },
        }
    }
}

impl CaptureSource for ScriptedSource {
    type Image = i64;

    fn begin_frames(&mut self) -> Result<(), RecorderError> {
        if self.policy.fail_activation {
            return Err(source_lost_error("SYNTHETIC activation failure".into()));
        }
        Ok(())
    }

    fn acquire(&mut self, timeout_ms: u32) -> Result<Option<Acquired<Self::Image>>, RecorderError> {
        if self.policy.require_nonblocking_after_copy
            && !self.copied.is_empty()
            && !self.policy.query_complete
            && timeout_ms != 0
        {
            return Err(internal_error(
                "A pending copy was blocked behind desktop acquisition.".into(),
            ));
        }
        if self.outstanding {
            return Err(internal_error(
                "the synthetic source was asked to acquire before release".to_string(),
            ));
        }
        let Some(step) = self.steps.pop_front() else {
            // Deterministic termination for the synchronous harness: the script is
            // done, so the test cancels the worker. `Ok(None)` is the documented
            // "no update within the polling bound" result.
            self.exhausted.store(true, Ordering::Release);
            return Ok(None);
        };
        match step {
            Step::ReplaySample(time) => {
                self.outstanding = true;
                Ok(Some(Acquired {
                    image: 0,
                    observation: FrameObservation {
                        last_mouse_update_time: time,
                        synthetic_replay_time: true,
                        ..FrameObservation::default()
                    },
                }))
            }
            Step::Timeout => Ok(None),
            Step::Delay(millis) => {
                std::thread::sleep(Duration::from_millis(millis));
                Ok(None)
            }
            Step::Failure => Err(source_lost_error(
                "SYNTHETIC acquisition failure".to_string(),
            )),
            Step::Panic => panic!("SYNTHETIC injected worker panic"),
            Step::PointerOnly(last_mouse_update_time) => {
                self.outstanding = true;
                Ok(Some(Acquired {
                    image: 0,
                    observation: FrameObservation {
                        last_present_time: 0,
                        last_mouse_update_time,
                        synthetic_cursor_time: false,
                        synthetic_replay_time: false,
                        accumulated_frames: 0,
                        protected_content_masked_out: false,
                        trace: None,
                    },
                }))
            }
            Step::SyntheticCursor(last_mouse_update_time) => {
                self.outstanding = true;
                Ok(Some(Acquired {
                    image: 0,
                    observation: FrameObservation {
                        last_present_time: 0,
                        last_mouse_update_time,
                        synthetic_cursor_time: true,
                        synthetic_replay_time: false,
                        accumulated_frames: 0,
                        protected_content_masked_out: false,
                        trace: None,
                    },
                }))
            }
            Step::Image {
                last_present,
                accumulated,
            } => {
                self.outstanding = true;
                Ok(Some(Acquired {
                    image: last_present,
                    observation: FrameObservation {
                        last_present_time: last_present,
                        last_mouse_update_time: 0,
                        synthetic_cursor_time: false,
                        synthetic_replay_time: false,
                        accumulated_frames: accumulated,
                        protected_content_masked_out: false,
                        trace: None,
                    },
                }))
            }
            Step::ImageAndPointer {
                last_present,
                last_mouse,
            } => {
                self.outstanding = true;
                Ok(Some(Acquired {
                    image: last_present,
                    observation: FrameObservation {
                        last_present_time: last_present,
                        last_mouse_update_time: last_mouse,
                        synthetic_cursor_time: false,
                        synthetic_replay_time: false,
                        accumulated_frames: 1,
                        protected_content_masked_out: false,
                        trace: None,
                    },
                }))
            }
        }
    }

    fn begin_copy(&mut self, image: &Self::Image, slot_index: usize) -> Result<(), RecorderError> {
        if self.policy.panic_in_begin_copy {
            panic!("SYNTHETIC copy submission panic");
        }
        if self.policy.fail_begin_copy {
            return Err(internal_error(
                "SYNTHETIC copy submission failure".to_string(),
            ));
        }
        self.copied.push(slot_index);
        let _ = image;
        Ok(())
    }

    fn query_complete(&mut self, _slot_index: usize) -> Result<bool, RecorderError> {
        Ok(self.policy.query_complete)
    }

    fn release_frame(&mut self) -> Result<(), RecorderError> {
        if !self.outstanding {
            return Err(internal_error(
                "the synthetic source was asked to release without an acquire".to_string(),
            ));
        }
        self.outstanding = false;
        if self.policy.fail_release {
            return Err(source_lost_error("SYNTHETIC release failure".to_string()));
        }
        Ok(())
    }

    fn readback(&mut self, _slot_index: usize) -> ReadbackOutcome {
        if let Some((entered, proceed)) = self.readback_gate.take() {
            entered.send(()).expect("verification observer is alive");
            proceed
                .recv_timeout(Duration::from_secs(5))
                .expect("verification must be released after cancellation");
            assert!(self.cancellation.is_cancelled());
        }
        if self.policy.panic_in_readback {
            panic!("SYNTHETIC injected verification panic");
        }
        if self.policy.cancel_readback {
            // Exactly what the native readback reports when it observes cancellation
            // between bounded map polls: the token is set, so the winner is a normal
            // stop rather than a failure.
            self.cancellation.cancel();
            return ReadbackOutcome::Cancelled;
        }
        if self.policy.fail_readback {
            return ReadbackOutcome::Failed(internal_error(
                "SYNTHETIC verification failure".to_string(),
            ));
        }
        ReadbackOutcome::Verified(ReadbackSample {
            row_pitch: 64 * 4,
            bytes: 64 * 4 * 48,
            valid_row_bytes: 64 * 4,
            hash_bytes: 64 * 4 * 48,
            padded: false,
            digest: 7,
            samples: [Default::default(); super::diagnostics::READBACK_PIXEL_SAMPLES],
        })
    }

    fn ticks_per_second(&self) -> u64 {
        self.ticks_per_second
    }

    fn sample_counter(&mut self) -> Result<i64, RecorderError> {
        self.counter += 1;
        Ok(self.counter)
    }

    fn description(&self) -> &NativeDescription {
        &self.description
    }

    fn payload_owner(&self) -> SharedPayload {
        Arc::clone(&self.payload) as SharedPayload
    }
}

/// Starts a real session backed by a synthetic source, keeping a weak view of the
/// payload owner so destruction can be observed.
fn synthetic_session_with_lifetime(
    steps: Vec<Step>,
    policy: ScriptPolicy,
    options: CaptureOptions,
) -> (super::WindowsCaptureSession, Weak<SyntheticPayload>) {
    let clock = Arc::new(MasterClock::default());
    let context = CaptureContext {
        clock,
        cancellation: CaptureCancellation::default(),
    };
    let exhausted = Arc::new(AtomicBool::new(false));
    let weak_slot: Arc<std::sync::Mutex<Option<Weak<SyntheticPayload>>>> =
        Arc::new(std::sync::Mutex::new(None));
    let weak_writer = Arc::clone(&weak_slot);
    let session = start_with_factory(
        synthetic_selection(),
        super::plan_owned_payload(64, 48, options.verify_pixels).expect("synthetic plan fits"),
        context,
        options,
        None,
        move |preparation: &CapturePreparation<'_>| {
            let source = ScriptedSource::new(
                steps,
                policy,
                Arc::clone(&exhausted),
                preparation.lifetime,
                preparation.cancellation.clone(),
            );
            if let Ok(mut slot) = weak_writer.lock() {
                *slot = Some(Arc::downgrade(&source.payload));
            }
            Ok(source)
        },
    )
    .expect("the synthetic session must start");
    let weak = weak_slot
        .lock()
        .expect("the weak slot is readable")
        .clone()
        .expect("the factory ran");
    (session, weak)
}

fn synthetic_session(
    steps: Vec<Step>,
    policy: ScriptPolicy,
    options: CaptureOptions,
) -> super::WindowsCaptureSession {
    synthetic_session_with_lifetime(steps, policy, options).0
}

#[test]
fn startup_preview_observes_the_only_surface_before_ui_transport_attaches() {
    let config = crate::preview::PreviewConfig::default();
    let hub = crate::preview::new_hub(config, 80).unwrap();
    let bootstrap = hub.subscribe();
    let mut session = start_with_factory(
        synthetic_selection(),
        super::plan_owned_payload(64, 48, false).unwrap(),
        CaptureContext {
            clock: Arc::new(MasterClock::default()),
            cancellation: CaptureCancellation::default(),
        },
        CaptureOptions::default(),
        Some(super::PreviewSetup {
            config,
            hub: Some(Arc::clone(&hub)),
        }),
        move |preparation| {
            Ok(ScriptedSource::new(
                images(1, 1_000_000),
                ScriptPolicy {
                    query_complete: true,
                    ..ScriptPolicy::default()
                },
                Arc::new(AtomicBool::new(false)),
                preparation.lifetime,
                preparation.cancellation.clone(),
            ))
        },
    )
    .unwrap();
    // The source intentionally has no D3D device. Its first surface must reach
    // preview generation and publish that explicit error, not disappear before
    // attachment. This tests startup ordering, not real GPU pixels.
    let image = hub.wait_for_image(Duration::from_secs(2)).unwrap().unwrap();
    assert!(image.error().is_some());
    assert_eq!(image.session_generation, 80);
    let transport_subscription = hub.subscribe();
    drop(bootstrap);
    assert_eq!(hub.subscriber_count(), 1);
    session.stop().unwrap();
    drop(transport_subscription);
}

#[test]
fn recording_preparation_does_not_acquire_or_evict_the_first_image() {
    let mut session = synthetic_session(
        images(1, 1_000_000),
        ScriptPolicy {
            query_complete: true,
            ..ScriptPolicy::default()
        },
        CaptureOptions {
            defer_frames: true,
            ..CaptureOptions::default()
        },
    );
    let surface = session.preparation_surface().unwrap().unwrap();
    assert!(surface.is_preparation());
    std::thread::sleep(Duration::from_millis(30)); // downstream preparation
    assert!(session.try_frame().unwrap().is_none());
    assert_eq!(
        session
            .shared
            .counters
            .acquire_success
            .load(Ordering::Acquire),
        0
    );
    assert_eq!(session.stats().counters.ready_evictions, 0);
    assert!(
        session.begin_frames().is_err(),
        "a preparation lease cannot race a copy"
    );
    drop(surface);
    assert_eq!(session.shared.pool.counts().free, SLOT_COUNT);
    session.begin_frames().unwrap();
    let frames = consume(&mut session, 1, Duration::from_secs(2));
    assert_eq!(frames.len(), 1);
    assert!(!frames[0].surface.is_preparation());
    assert_eq!(
        session
            .shared
            .counters
            .acquire_success
            .load(Ordering::Acquire),
        1
    );
    assert_eq!(session.stats().counters.ready_evictions, 0);
    session.stop().unwrap();
}

#[test]
fn cancelling_before_capture_is_armed_joins_without_acquiring_images() {
    let (mut session, payload) = synthetic_session_with_lifetime(
        images(5, 1_000_000),
        ScriptPolicy::default(),
        CaptureOptions {
            defer_frames: true,
            ..CaptureOptions::default()
        },
    );
    session.stop().unwrap();
    assert_eq!(
        session
            .shared
            .counters
            .acquire_success
            .load(Ordering::Acquire),
        0
    );
    assert!(payload.upgrade().is_none());
    assert!(session.preparation_surface().unwrap().is_none());
}

#[test]
fn deferred_activation_failure_is_sticky_and_never_acquires_an_image() {
    let mut session = synthetic_session(
        images(5, 1_000_000),
        ScriptPolicy {
            fail_activation: true,
            ..ScriptPolicy::default()
        },
        CaptureOptions {
            defer_frames: true,
            ..CaptureOptions::default()
        },
    );
    assert!(session.try_frame().unwrap().is_none());
    session.begin_frames().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let error = loop {
        match session.try_frame() {
            Err(error) => break error,
            Ok(_) if Instant::now() < deadline => std::thread::yield_now(),
            Ok(_) => panic!("activation failure was not surfaced"),
        }
    };
    assert_eq!(error.code, ErrorCode::SourceLost);
    assert_eq!(
        session
            .shared
            .counters
            .acquire_success
            .load(Ordering::Acquire),
        0
    );
    assert_eq!(session.stop().unwrap_err(), error);
}

/// A shared state for one synthetic session, so a test can prepare (for example
/// poison) a lock before the worker runs.
fn synthetic_shared_state(verify_pixels: bool) -> (Arc<SharedState>, Arc<PayloadLifetime>) {
    let lifetime = PayloadLifetime::new();
    let shared = Arc::new(SharedState::new(
        64,
        48,
        verify_pixels,
        Arc::clone(&lifetime),
        super::plan_owned_payload(64, 48, verify_pixels).expect("synthetic plan fits"),
    ));
    (shared, lifetime)
}

/// Runs the worker on its own thread with a synthetic source and stops it once the
/// script is exhausted or the worker has stopped itself.
fn run_script(steps: Vec<Step>, policy: ScriptPolicy, verify_pixels: bool) -> Arc<SharedState> {
    let (shared, _, panicked) = run_script_detailed(steps, policy, verify_pixels);
    assert!(
        !panicked,
        "this harness expects the worker to finish without panicking"
    );
    shared
}

/// As [`run_script`], but also reports whether the worker unwound.
///
/// The direct harness has no panic boundary (that lives at the session's thread
/// boundary), so the unwind tests need the join outcome instead of an assertion.
fn run_script_detailed(
    steps: Vec<Step>,
    policy: ScriptPolicy,
    verify_pixels: bool,
) -> (Arc<SharedState>, Arc<PayloadLifetime>, bool) {
    let (shared, lifetime) = synthetic_shared_state(verify_pixels);
    run_script_on_detailed(shared, lifetime, steps, policy, verify_pixels)
}

/// Runs the worker against a prepared shared state.
fn run_script_on(
    shared: Arc<SharedState>,
    lifetime: Arc<PayloadLifetime>,
    steps: Vec<Step>,
    policy: ScriptPolicy,
    verify_pixels: bool,
) -> Arc<SharedState> {
    let (shared, _, panicked) =
        run_script_on_detailed(shared, lifetime, steps, policy, verify_pixels);
    assert!(
        !panicked,
        "this harness expects the worker to finish without panicking"
    );
    shared
}

fn run_script_on_detailed(
    shared: Arc<SharedState>,
    lifetime: Arc<PayloadLifetime>,
    steps: Vec<Step>,
    policy: ScriptPolicy,
    verify_pixels: bool,
) -> (Arc<SharedState>, Arc<PayloadLifetime>, bool) {
    let cancellation = CaptureCancellation::default();
    let worker_cancellation = cancellation.clone();
    let context = WorkerContext {
        shared: Arc::clone(&shared),
        clock: Arc::new(MasterClock::default()),
        cancellation: cancellation.clone(),
        verify_pixels,
        preview: None,
    };
    let exhausted = Arc::new(AtomicBool::new(false));
    let source = ScriptedSource::new(
        steps,
        policy,
        Arc::clone(&exhausted),
        &lifetime,
        cancellation,
    );
    let (ready_tx, _ready_rx) = sync_channel(1);
    let worker = std::thread::spawn(move || super::worker::run(source, context, ready_tx));

    // A script can end in a terminal step, in which case the worker stops itself
    // before the script is exhausted; either way the harness waits for a stable state.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !exhausted.load(Ordering::Acquire)
        && !shared.worker_finished.load(Ordering::Acquire)
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(1));
    }
    worker_cancellation.cancel();
    let panicked = worker.join().is_err();
    assert!(
        panicked
            || exhausted.load(Ordering::Acquire)
            || shared.worker_finished.load(Ordering::Acquire),
        "the worker must have consumed the script, stopped itself or unwound"
    );
    (shared, lifetime, panicked)
}

fn images(count: u32, first_ticks: i64) -> Vec<Step> {
    (0..count)
        .map(|index| Step::Image {
            last_present: first_ticks + i64::from(index) * 1_000_000,
            accumulated: 1,
        })
        .collect()
}

/// Collects up to `wanted` frames with a bounded wait.
fn consume(
    session: &mut super::WindowsCaptureSession,
    wanted: usize,
    timeout: Duration,
) -> Vec<frame_capture::CapturedFrame<crate::surface::WindowsSurface>> {
    let mut frames = Vec::new();
    let deadline = Instant::now() + timeout;
    while frames.len() < wanted && Instant::now() < deadline {
        match session.try_frame() {
            Ok(Some(frame)) => frames.push(frame),
            Ok(None) => std::thread::sleep(Duration::from_millis(1)),
            Err(error) => panic!("the synthetic session must not fail: {error}"),
        }
    }
    frames
}

#[test]
fn unsupported_targets_and_frame_rates_fail_before_any_native_work() {
    use frame_contracts::{CaptureTarget, DisplayId, GameTarget, Region};

    assert_eq!(
        validate_request(
            &CaptureTarget::Window(frame_contracts::WindowId("w".into())),
            FpsMode::AutoMatchSource
        )
        .unwrap_err()
        .code,
        ErrorCode::NotImplemented
    );
    assert_eq!(
        validate_request(
            &CaptureTarget::Region(Region {
                display_id: DisplayId("d".into()),
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            }),
            FpsMode::AutoMatchSource
        )
        .unwrap_err()
        .code,
        ErrorCode::NotImplemented
    );
    assert_eq!(
        validate_request(
            &CaptureTarget::Game(GameTarget {
                process_id: 1,
                app_identifier: None,
            }),
            FpsMode::AutoMatchSource
        )
        .unwrap_err()
        .code,
        ErrorCode::NotImplemented
    );
    assert_eq!(
        validate_request(
            &CaptureTarget::Display(DisplayId("d".into())),
            FpsMode::Fixed(60.0)
        )
        .unwrap_err()
        .code,
        ErrorCode::NotImplemented
    );
    for invalid in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert_eq!(
            validate_request(
                &CaptureTarget::Display(DisplayId("d".into())),
                FpsMode::Fixed(invalid)
            )
            .unwrap_err()
            .code,
            ErrorCode::InvalidRequest,
            "fixed rate {invalid} must be rejected as an invalid request"
        );
    }
    assert!(
        validate_request(
            &CaptureTarget::Display(DisplayId("d".into())),
            FpsMode::AutoMatchSource
        )
        .is_ok()
    );
}

#[test]
fn probe_options_are_validated_before_preparation() {
    let id = frame_contracts::DisplayId("\\\\?\\DISPLAY#SYNTHETIC#1".to_string());
    assert_eq!(
        capture_probe(&id, 0, 0, false).unwrap_err().code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        capture_probe(&id, 3601, 0, false).unwrap_err().code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        capture_probe(&id, 10, 10_001, false).unwrap_err().code,
        ErrorCode::InvalidRequest
    );
}

// ---------------------------------------------------------------------------------
// FRAME-0103-R1 reproduced regression 1: payload destruction after a no-lease stop.
// ---------------------------------------------------------------------------------

#[test]
fn a_no_lease_stop_destroys_the_payload_owner_while_the_session_stays_alive() {
    let (mut session, weak) = synthetic_session_with_lifetime(
        images(4, 1_000_000),
        ScriptPolicy {
            query_complete: true,
            ..ScriptPolicy::default()
        },
        CaptureOptions::default(),
    );
    assert!(
        weak.upgrade().is_some(),
        "the payload owner must exist while the session is running"
    );
    let running = session.diagnostics().expect("diagnostics");
    assert!(
        running.contains("owners_live=1"),
        "the lifetime counter must observe the live owner:\n{running}"
    );

    // Consume and release every lease: no lease is held at stop.
    let frames = consume(&mut session, 2, Duration::from_secs(2));
    assert_eq!(frames.len(), 2);
    for frame in frames {
        drop(frame.surface);
    }

    session.stop().expect("teardown must succeed");
    // The session is still in scope and its diagnostics still work, but the payload is
    // gone: destruction is observed, not inferred from slot labels.
    assert!(
        weak.upgrade().is_none(),
        "no lease means the payload owner must be destroyed"
    );
    let report = session.diagnostics().expect("diagnostics after stop");
    assert!(
        report.contains("owners_live=0") && report.contains("session_reference_held=false"),
        "diagnostics must report the destroyed payload:\n{report}"
    );
    assert!(report.contains("reconcile verdict: every checked identity holds"));
}

#[test]
fn a_held_lease_keeps_its_backing_alive_until_it_is_released() {
    let (mut session, weak) = synthetic_session_with_lifetime(
        images(4, 1_000_000),
        ScriptPolicy {
            query_complete: true,
            ..ScriptPolicy::default()
        },
        CaptureOptions::default(),
    );
    let frames = consume(&mut session, 1, Duration::from_secs(2));
    assert_eq!(frames.len(), 1);

    session.stop().expect("teardown must succeed");
    assert!(
        weak.upgrade().is_some(),
        "a held lease must keep its backing alive across stop"
    );
    let report = session.diagnostics().expect("diagnostics after stop");
    assert!(report.contains("owners_live=1"), "{report}");

    drop(frames);
    assert!(
        weak.upgrade().is_none(),
        "releasing the last lease must destroy the backing"
    );
    let report = session
        .diagnostics()
        .expect("diagnostics after the lease release");
    assert!(report.contains("owners_live=0"), "{report}");
}

// ---------------------------------------------------------------------------------
// FRAME-0103-R1 reproduced regression 2: a verification failure must not orphan a slot.
// ---------------------------------------------------------------------------------

#[test]
fn a_verification_failure_returns_its_slot_exactly_once_and_reconciles() {
    let shared = run_script(
        images(3, 1_000_000),
        ScriptPolicy {
            query_complete: true,
            fail_readback: true,
            ..ScriptPolicy::default()
        },
        true,
    );
    let counters = shared.counters.snapshot();
    let slots = shared.pool.counts();

    assert_eq!(counters.copy_submissions, 1);
    assert_eq!(counters.copy_completions, 1);
    assert_eq!(counters.readback_failures, 1);
    assert_eq!(
        counters.discarded_on_verification_failure, 1,
        "the orphaned-slot path must be counted, not silent"
    );
    assert_eq!(
        slots.occupied(),
        0,
        "no slot may stay occupied after the failure"
    );
    assert_eq!(slots.free, SLOT_COUNT);
    assert!(
        counters
            .identities(shared.ready.lock().unwrap().stats(), slots)
            .all(),
        "identities must hold after a verification failure"
    );
    let terminal = shared.terminal_error().expect("the failure is terminal");
    assert_eq!(terminal.code, ErrorCode::Internal);
    assert!(terminal.message.contains("SYNTHETIC verification failure"));
}

// ---------------------------------------------------------------------------------
// FRAME-0103-R1 reproduced regression 3: the primary error must survive cleanup.
// ---------------------------------------------------------------------------------

#[test]
fn a_copy_failure_stays_primary_when_release_also_fails() {
    let shared = run_script(
        images(1, 1_000_000),
        ScriptPolicy {
            fail_begin_copy: true,
            fail_release: true,
            ..ScriptPolicy::default()
        },
        false,
    );
    let counters = shared.counters.snapshot();
    assert_eq!(counters.copy_failures, 1);
    let terminal = shared.terminal_error().expect("the failure is terminal");
    assert_eq!(
        terminal.code,
        ErrorCode::Internal,
        "the copy failure's code must not be replaced by the release failure's"
    );
    assert!(
        terminal
            .message
            .contains("SYNTHETIC copy submission failure"),
        "the primary message must be retained: {}",
        terminal.message
    );
    assert!(
        terminal.message.contains("SYNTHETIC release failure"),
        "the cleanup context must be attached: {}",
        terminal.message
    );
    assert_eq!(shared.pool.counts().free, SLOT_COUNT);
    assert!(
        counters
            .identities(shared.ready.lock().unwrap().stats(), shared.pool.counts())
            .all()
    );
}

#[test]
fn a_release_failure_alone_is_still_reported() {
    let shared = run_script(
        images(1, 1_000_000),
        ScriptPolicy {
            query_complete: true,
            fail_release: true,
            ..ScriptPolicy::default()
        },
        false,
    );
    let terminal = shared
        .terminal_error()
        .expect("a release failure is a terminal error");
    assert!(terminal.message.contains("SYNTHETIC release failure"));
    assert_eq!(shared.pool.counts().free, SLOT_COUNT);
}

// ---------------------------------------------------------------------------------
// FRAME-0103-R1 reproduced regression 4: a dead worker is never an empty queue.
// ---------------------------------------------------------------------------------

#[test]
fn a_post_readiness_panic_is_sticky_and_survives_stop() {
    let mut session = synthetic_session(
        vec![Step::Panic],
        ScriptPolicy::default(),
        CaptureOptions::default(),
    );

    // The panic happens after readiness; the session must report it promptly instead
    // of returning `Ok(None)` forever.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut observed = None;
    while observed.is_none() && Instant::now() < deadline {
        match session.try_frame() {
            Ok(Some(_)) => panic!("a panicking worker cannot deliver a frame"),
            Ok(None) => std::thread::sleep(Duration::from_millis(1)),
            Err(error) => observed = Some(error),
        }
    }
    let error = observed.expect("the worker panic must become a sticky error");
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(error.message.contains("panicked"), "{}", error.message);

    // The error stays sticky through stop, and stop joins rather than detaching.
    assert!(session.try_frame().is_err());
    let stop_error = session
        .stop()
        .expect_err("stop reports the failure it tore down");
    assert_eq!(stop_error.code, ErrorCode::Internal);
    assert!(
        stop_error.message.contains("panicked"),
        "{}",
        stop_error.message
    );
    assert!(session.stop().is_err());
    let report = session.diagnostics().expect("diagnostics after a panic");
    assert!(report.contains("terminal error: code=Internal"));
}

// ---------------------------------------------------------------------------------
// FRAME-0103-R1 reproduced regression 5: held diagnostics must not block dequeue.
// ---------------------------------------------------------------------------------

#[test]
fn dequeue_does_not_block_while_diagnostic_locks_are_held() {
    let (mut session, _weak) = synthetic_session_with_lifetime(
        images(4, 1_000_000),
        ScriptPolicy {
            query_complete: true,
            ..ScriptPolicy::default()
        },
        CaptureOptions::default(),
    );
    let frames = consume(&mut session, 1, Duration::from_secs(2));
    assert_eq!(frames.len(), 1);

    // Hold every diagnostic-side lock the report uses, then dequeue: the dequeue and
    // the report must both complete promptly, or report contention explicitly.
    // Guards are taken from a cloned Arc so the session itself stays mutable.
    let shared = Arc::clone(&session.shared);
    let _log = shared.log.lock().expect("log");
    let _native = shared.native.lock().expect("native");
    let _clock = shared.clock_evidence.lock().expect("clock");
    let _readback = shared.readback.lock().expect("readback");
    let _queue = shared.ready.lock().expect("ready");

    let started = Instant::now();
    let outcome = session.try_frame();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(100),
        "try_frame blocked for {elapsed:?} while diagnostic locks were held"
    );
    match outcome {
        // Either contention (no ready update observed now) or a delivered frame is
        // acceptable; blocking is not.
        Ok(Some(frame)) => drop(frame.surface),
        Ok(None) => {}
        Err(error) => panic!("held diagnostic locks must not be an error: {error}"),
    }

    let report_started = Instant::now();
    let report = session.diagnostics().expect("diagnostics under contention");
    assert!(
        report_started.elapsed() < Duration::from_secs(1),
        "diagnostics blocked on held locks"
    );
    assert!(
        report.contains("could not be read without blocking"),
        "contention must be reported explicitly:\n{report}"
    );
    assert!(
        report.contains("queue: unavailable at this snapshot"),
        "a contended queue must not print a substitute value:\n{report}"
    );
}

// ---------------------------------------------------------------------------------
// Exit-path, pressure and teardown behaviour preserved from FRAME-0103.
// ---------------------------------------------------------------------------------

#[test]
fn replay_still_samples_preserve_source_identity_and_copy_accounting() {
    let shared = run_script(
        vec![
            Step::Image {
                last_present: 1_000_000,
                accumulated: 1,
            },
            Step::ReplaySample(2_000_000),
            Step::ReplaySample(3_000_000),
        ],
        ScriptPolicy::default(),
        false,
    );
    let counters = shared.counters.snapshot();
    assert_eq!(counters.native_desktop_presentations, 1);
    assert_eq!(counters.replay_samples, 2);
    assert_eq!(counters.pointer_only, 0);
    assert_eq!(counters.copy_submissions, 3);
    assert!(
        counters
            .identities(shared.ready.lock().unwrap().stats(), shared.pool.counts())
            .all()
    );
}

#[test]
fn pointer_only_updates_become_frames_and_repeated_observations_do_not() {
    let mut steps = vec![
        Step::PointerOnly(1_000_000),
        Step::PointerOnly(2_000_000),
        Step::Timeout,
    ];
    steps.push(Step::Image {
        last_present: 5_000_000,
        accumulated: 1,
    });
    steps.push(Step::Image {
        last_present: 5_000_000,
        accumulated: 1,
    });
    let shared = run_script(steps, ScriptPolicy::default(), false);
    let counters = shared.counters.snapshot();

    assert_eq!(counters.pointer_only, 2);
    assert_eq!(counters.updates_with_image, 2);
    assert_eq!(counters.repeats, 1);
    assert_eq!(
        counters.copy_submissions, 3,
        "cursor movement is copied; a repeat is not"
    );
    assert_eq!(counters.copy_completions, 0, "the query never completed");
    assert_eq!(counters.ready_published, 0);
    assert_eq!(counters.shutdown_discarded_pending, 3);
    assert_eq!(shared.pool.counts().copy_pending, 0);
    assert_eq!(shared.pool.counts().free, SLOT_COUNT);
    assert!(
        counters
            .identities(shared.ready.lock().unwrap().stats(), shared.pool.counts())
            .all()
    );
}

#[test]
fn pending_queries_publish_nothing_until_the_payload_says_complete() {
    let shared = run_script(images(3, 1_000_000), ScriptPolicy::default(), false);
    let counters = shared.counters.snapshot();

    assert_eq!(counters.copy_submissions, 3);
    assert_eq!(counters.copy_completions, 0);
    assert_eq!(counters.ready_published, 0);
    assert_eq!(counters.pool_exhausted, 0);
    assert_eq!(counters.shutdown_discarded_pending, 3);
    assert_eq!(shared.pool.counts().occupied(), 0);
    assert!(
        counters
            .identities(shared.ready.lock().unwrap().stats(), shared.pool.counts())
            .all()
    );
}

#[test]
fn pending_copy_on_a_quiet_source_keeps_completion_polling_without_acquire_waits() {
    let shared = run_script(
        vec![
            Step::Image {
                last_present: 1_000_000,
                accumulated: 1,
            },
            Step::Timeout,
            Step::Timeout,
        ],
        ScriptPolicy {
            require_nonblocking_after_copy: true,
            ..ScriptPolicy::default()
        },
        false,
    );
    assert!(shared.terminal_error().is_none());
    let snapshot = shared.counters.snapshot();
    assert_eq!(snapshot.copy_submissions, 1);
    assert_eq!(snapshot.shutdown_discarded_pending, 1);
    assert_eq!(snapshot.acquire_failures, 0);
    assert!(
        snapshot
            .identities(shared.ready.lock().unwrap().stats(), shared.pool.counts())
            .all()
    );
}

#[test]
fn completed_cursor_images_do_not_inflate_captured_unique_presentations() {
    let shared = run_script(
        vec![
            Step::Image {
                last_present: 1_000_000,
                accumulated: 1,
            },
            Step::Image {
                last_present: 1_000_000,
                accumulated: 1,
            },
            Step::PointerOnly(2_000_000),
            Step::SyntheticCursor(3_000_000),
        ],
        ScriptPolicy {
            query_complete: true,
            ..ScriptPolicy::default()
        },
        false,
    );
    let snapshot = shared.counters.snapshot();
    assert_eq!(snapshot.native_desktop_presentations, 1);
    assert_eq!(snapshot.captured_desktop_presentations, 1);
    assert_eq!(snapshot.copy_completions, 3);
}

#[test]
fn native_presentation_identity_excludes_cursor_repeats_and_accumulation_under_pressure() {
    let mut steps = vec![
        Step::PointerOnly(1_000_000),
        Step::SyntheticCursor(2_000_000),
    ];
    for ticks in [
        3_000_000, 3_000_000, 4_000_000, 5_000_000, 6_000_000, 7_000_000,
    ] {
        steps.push(Step::Image {
            last_present: ticks,
            accumulated: 3,
        });
    }
    // Queries stay pending: native observations continue beyond the four-slot pool.
    let shared = run_script(steps, ScriptPolicy::default(), false);
    let snapshot = shared.counters.snapshot();
    let public = super::CaptureCounters::from_snapshot(&snapshot);
    assert_eq!(public.native_desktop_presentations, 5);
    assert_eq!(public.captured_desktop_presentations, 0);
    assert_eq!(snapshot.updates_with_image, 6);
    assert_eq!(snapshot.pointer_only, 2);
    assert_eq!(snapshot.repeats, 1);
    assert_eq!(snapshot.pool_exhausted, 3);
    assert_eq!(snapshot.copy_submissions, 4);
    assert_eq!(snapshot.upstream_accumulated_updates, 12);
    assert!(
        snapshot
            .identities(shared.ready.lock().unwrap().stats(), shared.pool.counts())
            .all()
    );
}

#[test]
fn pool_and_ready_queue_stay_bounded_under_completed_pressure() {
    let policy = ScriptPolicy {
        query_complete: true,
        ..ScriptPolicy::default()
    };
    let shared = run_script(images(6, 1_000_000), policy, false);
    let counters = shared.counters.snapshot();
    let slots = shared.pool.counts();
    let queue = shared.ready.lock().unwrap().stats();

    assert_eq!(counters.copy_submissions, 6);
    assert_eq!(counters.copy_completions, 6);
    assert_eq!(counters.pool_exhausted, 0);
    assert_eq!(queue.capacity, 2);
    assert_eq!(queue.depth, 2, "the ready queue never exceeds its capacity");
    assert_eq!(
        counters.ready_evictions, 4,
        "DropOldest evicts older frames"
    );
    assert_eq!(queue.dropped, 4);
    assert_eq!(counters.ready_published, 6);
    assert!(slots.occupied() <= SLOT_COUNT);
    assert_eq!(slots.occupied() + slots.free, SLOT_COUNT);
    assert!(counters.identities(queue, slots).all());
}

#[test]
fn unfinished_copies_hold_every_slot_and_later_updates_are_dropped() {
    let shared = run_script(images(10, 1_000_000), ScriptPolicy::default(), false);
    let counters = shared.counters.snapshot();
    let slots = shared.pool.counts();

    assert_eq!(slots.occupied() + slots.free, SLOT_COUNT);
    assert_eq!(counters.copy_submissions, SLOT_COUNT as u64);
    assert_eq!(
        counters.pool_exhausted, 6,
        "later updates are dropped, not pooled"
    );
    assert_eq!(counters.copy_submissions + counters.pool_exhausted, 10);
    assert_eq!(counters.shutdown_discarded_pending, SLOT_COUNT as u64);
    assert!(
        counters
            .identities(shared.ready.lock().unwrap().stats(), slots)
            .all()
    );
}

#[test]
fn terminal_acquisition_failure_is_explicit() {
    let failure = run_script(vec![Step::Failure], ScriptPolicy::default(), false);
    let terminal = failure
        .terminal_error()
        .expect("a terminal error is recorded");
    assert_eq!(terminal.code, ErrorCode::SourceLost);
    assert!(terminal.message.contains("SYNTHETIC acquisition failure"));
}

#[test]
fn a_terminal_error_takes_precedence_over_queued_frames() {
    let policy = ScriptPolicy {
        query_complete: true,
        ..ScriptPolicy::default()
    };
    let script = vec![
        Step::Image {
            last_present: 1_000_000,
            accumulated: 1,
        },
        Step::Image {
            last_present: 2_000_000,
            accumulated: 1,
        },
        Step::Failure,
    ];
    let mut session = synthetic_session(script, policy, CaptureOptions::default());

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut first_error = None;
    while first_error.is_none() && Instant::now() < deadline {
        match session.try_frame() {
            Ok(Some(_)) => {}
            Ok(None) => std::thread::sleep(Duration::from_millis(1)),
            Err(error) => first_error = Some(error),
        }
    }
    let error = first_error.expect("the terminal error must surface through try_frame");
    assert_eq!(error.code, ErrorCode::SourceLost);
    assert!(error.message.contains("SYNTHETIC acquisition failure"));

    assert!(session.try_frame().is_err());
    let stop = session.stop();
    assert_eq!(
        stop.expect_err("stop reports the failure it tore down")
            .code,
        ErrorCode::SourceLost
    );
    assert!(session.stop().is_err());
}

#[test]
fn a_backwards_presentation_timestamp_is_a_source_loss() {
    let shared = run_script(
        vec![
            Step::Image {
                last_present: 2_000_000,
                accumulated: 1,
            },
            Step::Image {
                last_present: 1_000_000,
                accumulated: 1,
            },
        ],
        ScriptPolicy::default(),
        false,
    );
    let counters = shared.counters.snapshot();
    assert_eq!(counters.discontinuities, 1);
    let terminal = shared
        .terminal_error()
        .expect("a discontinuity is terminal");
    assert_eq!(terminal.code, ErrorCode::SourceLost);
    assert!(terminal.message.contains("moved backwards"));
}

#[test]
fn cursor_qpc_ahead_of_next_presentation_drops_only_the_late_image() {
    let shared = run_script(
        vec![
            Step::ImageAndPointer {
                last_present: 137_102_030_000,
                last_mouse: 137_102_048_392,
            },
            Step::Image {
                last_present: 137_102_038_831,
                accumulated: 1,
            },
            Step::Image {
                last_present: 137_102_048_393,
                accumulated: 1,
            },
        ],
        ScriptPolicy::default(),
        false,
    );
    let counters = shared.counters.snapshot();
    assert_eq!(counters.updates_with_image, 3);
    assert_eq!(counters.stale_visual, 1);
    assert_eq!(counters.discontinuities, 0);
    assert_eq!(counters.copy_submissions, 2);
    assert!(shared.terminal_error().is_none());
    assert!(
        counters
            .identities(shared.ready.lock().unwrap().stats(), shared.pool.counts())
            .all()
    );
}

#[test]
fn timeout_sample_does_not_create_a_false_native_pointer_regression() {
    let shared = run_script(
        vec![
            Step::PointerOnly(25_000_000),
            Step::SyntheticCursor(25_200_000),
            Step::PointerOnly(25_100_000),
            Step::PointerOnly(25_300_000),
        ],
        ScriptPolicy::default(),
        false,
    );
    let counters = shared.counters.snapshot();
    assert_eq!(counters.pointer_only, 4);
    assert_eq!(counters.stale_visual, 1);
    assert_eq!(counters.discontinuities, 0);
    assert_eq!(counters.copy_submissions, 3);
    assert!(shared.terminal_error().is_none());
    assert!(
        counters
            .identities(shared.ready.lock().unwrap().stats(), shared.pool.counts())
            .all()
    );
}

#[test]
fn upstream_accumulated_updates_are_counted_without_inventing_frames() {
    let shared = run_script(
        vec![Step::Image {
            last_present: 1_000_000,
            accumulated: 4,
        }],
        ScriptPolicy::default(),
        false,
    );
    let counters = shared.counters.snapshot();
    // Four upstream updates arrived while the previous frame was processed; three of
    // them were never delivered to this consumer, and none is fabricated here.
    assert_eq!(counters.upstream_accumulated_updates, 3);
    assert_eq!(counters.copy_submissions, 1);
}

#[test]
fn a_session_delivers_bounded_leases_and_reconciles_after_stop() {
    let mut session = synthetic_session(
        images(6, 1_000_000),
        ScriptPolicy {
            query_complete: true,
            ..ScriptPolicy::default()
        },
        CaptureOptions::default(),
    );

    let mut timestamps = Vec::new();
    let mut leases = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    while timestamps.len() < 2 && Instant::now() < deadline {
        match session.try_frame() {
            Ok(Some(frame)) => {
                timestamps.push(frame.timestamp.0);
                leases.push(frame.surface);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(1)),
            Err(error) => panic!("the synthetic session must not fail: {error}"),
        }
    }
    assert_eq!(timestamps.len(), 2, "two frames must be delivered");
    assert!(
        timestamps[0] < timestamps[1],
        "accepted timestamps must strictly increase: {timestamps:?}"
    );
    assert_eq!(session.shared.pool.counts().consumer_owned, 2);

    let stop = session.stop();
    assert!(stop.is_ok(), "teardown must succeed: {stop:?}");
    assert!(session.stop().is_ok(), "stop must be idempotent");
    for lease in &leases {
        assert_eq!(lease.slot_state(), SlotState::ConsumerOwned);
    }
    assert_eq!(session.shared.pool.counts().consumer_owned, 2);

    let report = session.diagnostics().expect("diagnostics must render");
    assert!(report.contains("not a recording"));
    assert!(report.contains("reconcile verdict: every checked identity holds"));
    assert!(report.contains("SYNTHETIC (not hardware)"));
    drop(leases);
    assert_eq!(session.shared.pool.counts().free, SLOT_COUNT);
}

#[test]
fn a_fresh_session_is_isolated_from_previous_tokens_queues_and_leases() {
    let mut first = synthetic_session(
        images(4, 1_000_000),
        ScriptPolicy {
            query_complete: true,
            ..ScriptPolicy::default()
        },
        CaptureOptions::default(),
    );
    let mut first_lease = None;
    let mut first_timestamp = None;
    let deadline = Instant::now() + Duration::from_secs(2);
    while first_lease.is_none() && Instant::now() < deadline {
        if let Ok(Some(frame)) = first.try_frame() {
            first_timestamp = Some(frame.timestamp.0);
            first_lease = Some(frame.surface);
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    let held = first_lease.expect("the first session must deliver a frame");
    let first_timestamp = first_timestamp.expect("the first frame has a timestamp");
    let first_id = first.shared.session_id;
    let _ = first.stop();

    // A clearly different native counter base, so the two sessions' timelines cannot
    // be confused even though each is anchored to its own first observation.
    let mut second = synthetic_session(
        images(4, 90_000_000),
        ScriptPolicy {
            query_complete: true,
            ..ScriptPolicy::default()
        },
        CaptureOptions::default(),
    );
    let second_id = second.shared.session_id;
    assert_ne!(first_id, second_id, "sessions must be distinguishable");

    assert_eq!(
        first.shared.pool.counts().consumer_owned,
        1,
        "the first session still holds one lease"
    );
    assert_eq!(
        second.shared.pool.counts().consumer_owned,
        0,
        "the new session must not inherit a lease from the previous one"
    );

    let mut second_frame = None;
    let deadline = Instant::now() + Duration::from_secs(2);
    while second_frame.is_none() && Instant::now() < deadline {
        if let Ok(Some(frame)) = second.try_frame() {
            second_frame = Some(frame);
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    let frame = second_frame.expect("the second session must deliver a frame");
    assert!(
        first_timestamp < 1_000_000_000,
        "the first session's timestamp {first_timestamp} must follow its own script"
    );
    assert!(
        frame.timestamp.0 > 5_000_000_000,
        "the new session's timestamp {} must follow its own script",
        frame.timestamp.0
    );
    drop(frame.surface);
    let _ = second.stop();

    drop(held);
    assert_eq!(first.shared.pool.counts().free, SLOT_COUNT);
    assert_eq!(second.shared.pool.counts().free, SLOT_COUNT);
}

// ---------------------------------------------------------------------------------
// Preparation outcomes, kept separate from worker death.
// ---------------------------------------------------------------------------------

#[test]
fn preparation_cancellation_and_failure_leave_no_session_running() {
    // Cancelled before preparation starts.
    let cancellation = CaptureCancellation::default();
    cancellation.cancel();
    let error = start_with_factory(
        synthetic_selection(),
        super::plan_owned_payload(64, 48, false).expect("synthetic plan fits"),
        CaptureContext {
            clock: Arc::new(MasterClock::default()),
            cancellation: cancellation.clone(),
        },
        CaptureOptions::default(),
        None,
        |preparation: &CapturePreparation<'_>| {
            assert!(preparation.cancellation.is_cancelled());
            Err::<ScriptedSource, _>(super::timing::preparation_cancelled())
        },
    )
    .expect_err("a cancelled preparation must not produce a session");
    assert_eq!(error.code, ErrorCode::InvalidTransition);

    // Cancelled while the factory is preparing.
    let cancellation = CaptureCancellation::default();
    let worker_token = cancellation.clone();
    let error = start_with_factory(
        synthetic_selection(),
        super::plan_owned_payload(64, 48, false).expect("synthetic plan fits"),
        CaptureContext {
            clock: Arc::new(MasterClock::default()),
            cancellation: cancellation.clone(),
        },
        CaptureOptions::default(),
        None,
        move |_preparation: &CapturePreparation<'_>| {
            worker_token.cancel();
            Err::<ScriptedSource, _>(super::timing::preparation_cancelled())
        },
    )
    .expect_err("a preparation failure must be reported");
    assert_eq!(error.code, ErrorCode::InvalidTransition);

    // A preparation failure keeps its own code and message.
    let error = start_with_factory(
        synthetic_selection(),
        super::plan_owned_payload(64, 48, false).expect("synthetic plan fits"),
        CaptureContext {
            clock: Arc::new(MasterClock::default()),
            cancellation: CaptureCancellation::default(),
        },
        CaptureOptions::default(),
        None,
        |_preparation: &CapturePreparation<'_>| {
            Err::<ScriptedSource, _>(internal_error("SYNTHETIC preparation failure".into()))
        },
    )
    .expect_err("a preparation failure must be reported");
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(error.message.contains("SYNTHETIC preparation failure"));
}

#[test]
fn diagnostics_keep_a_bounded_sample_log_and_state_native_limits() {
    let mut session = synthetic_session(
        images(80, 1_000_000),
        ScriptPolicy {
            query_complete: true,
            ..ScriptPolicy::default()
        },
        CaptureOptions {
            verify_pixels: true,
            hdr_float: false,
            show_cursor: true,
            defer_frames: false,
        },
    );
    let frames = consume(&mut session, 2, Duration::from_secs(2));
    assert_eq!(frames.len(), 2);
    drop(frames);
    let _ = session.stop();
    let report = session.diagnostics().expect("diagnostics");
    assert!(report.contains("not an atomic global snapshot"), "{report}");
    assert!(report.contains("slots: free="));
    assert!(report.contains("queue: depth="));
    assert!(report.contains("enabled=true"));
    assert!(report.contains("clock: qpc="));
    assert!(report.contains("sample seq="));
    assert!(report.contains("source: nominal_rate_hz=Some(60.0)"));
}

// ---------------------------------------------------------------------------------
// FRAME-0103-R2 findings.
// ---------------------------------------------------------------------------------

/// Poisons one session mutex by panicking while its guard is held.
fn poison<T>(mutex: &std::sync::Mutex<T>) {
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = mutex.lock().expect("the lock is not yet poisoned");
        panic!("SYNTHETIC poison injection");
    }));
    assert!(poisoned.is_err(), "the poisoning panic must be caught here");
    assert!(mutex.is_poisoned());
}

/// R2 finding 1: a worker that unwinds with an occupied slot must reclaim it.
#[test]
fn a_panic_with_a_pending_copy_reclaims_every_slot() {
    // One image with an incomplete query keeps a slot occupied, then the next
    // acquisition panics: the unwind path must return that slot exactly once.
    //
    // The direct harness has no panic boundary (that lives at the session's thread
    // boundary), so this test asserts the unwind accounting; the sticky error published
    // at that boundary is covered by the session-level panic test.
    let (shared, _lifetime, panicked) = run_script_detailed(
        vec![
            Step::Image {
                last_present: 1_000_000,
                accumulated: 1,
            },
            Step::Panic,
        ],
        ScriptPolicy::default(),
        false,
    );
    assert!(
        panicked,
        "the injected acquisition panic must unwind the worker"
    );
    let counters = shared.counters.snapshot();
    let slots = shared.pool.counts();

    assert_eq!(counters.copy_submissions, 1);
    assert_eq!(counters.copy_completions, 0);
    assert_eq!(counters.shutdown_discarded_pending, 1);
    assert_eq!(
        counters.discarded_on_unwind_pending, 0,
        "the in-flight copy is discarded through the pending accounting"
    );
    assert_eq!(slots.free, SLOT_COUNT, "every slot must be reclaimed");
    assert_eq!(slots.occupied(), 0);
    assert!(
        counters
            .identities(shared.ready.lock().unwrap().stats(), slots)
            .all(),
        "the unwind discard must be counted in the identities"
    );
}

#[test]
fn a_panic_during_copy_submission_reclaims_the_local_claim() {
    let (shared, _lifetime, panicked) = run_script_detailed(
        images(1, 1_000_000),
        ScriptPolicy {
            panic_in_begin_copy: true,
            ..Default::default()
        },
        false,
    );
    assert!(panicked);
    let counters = shared.counters.snapshot();
    let slots = shared.pool.counts();
    assert_eq!(slots.free, SLOT_COUNT);
    assert_eq!(counters.copy_submissions, 1);
    assert_eq!(counters.discarded_on_unwind_pending, 1);
    assert!(
        counters
            .identities(shared.ready.lock().unwrap().stats(), slots)
            .all()
    );
}

/// R2 finding 1: unwinding *without* reaching the normal shutdown path must also
/// reclaim the slot, and must count it as an unwind discard rather than hiding it.
#[test]
fn a_panic_during_verification_reclaims_the_completed_slot() {
    let (shared, _lifetime, panicked) = run_script_detailed(
        images(2, 1_000_000),
        ScriptPolicy {
            query_complete: true,
            panic_in_readback: true,
            ..ScriptPolicy::default()
        },
        true,
    );
    assert!(
        panicked,
        "the injected verification panic must unwind the worker"
    );
    let counters = shared.counters.snapshot();
    let slots = shared.pool.counts();

    assert_eq!(counters.copy_submissions, 1);
    assert_eq!(counters.copy_completions, 1);
    assert_eq!(
        counters.discarded_on_unwind_completed, 1,
        "the unwind discard of a completed copy must be counted"
    );
    assert_eq!(counters.shutdown_discarded_pending, 0);
    assert_eq!(slots.free, SLOT_COUNT);
    assert_eq!(slots.occupied(), 0);
    assert!(
        counters
            .identities(shared.ready.lock().unwrap().stats(), slots)
            .all(),
        "identities must hold after an unwind during verification"
    );
}

/// R2 finding 2: cancellation observed during verification stops the session normally.
#[test]
fn normal_cancellation_during_verification_stops_successfully() {
    // The delay keeps the worker busy until the control thread has completed the
    // readiness handshake, so the cancellation really lands after readiness.
    let mut steps = vec![Step::Delay(50)];
    steps.extend(images(3, 1_000_000));
    let mut session = synthetic_session(
        steps,
        ScriptPolicy {
            query_complete: true,
            cancel_readback: true,
            ..ScriptPolicy::default()
        },
        CaptureOptions {
            verify_pixels: true,
            hdr_float: false,
            show_cursor: true,
            defer_frames: false,
        },
    );
    // The first frame is verified and verification reports cancellation (the source
    // sets the session token, exactly as the native poll does when it observes
    // cancellation), so the worker stops on its own. The session must then tear down
    // successfully instead of reporting a preparation error.
    let deadline = Instant::now() + Duration::from_secs(2);
    while !session.shared.worker_finished.load(Ordering::Acquire) && Instant::now() < deadline {
        match session.try_frame() {
            Ok(_) => std::thread::sleep(Duration::from_millis(1)),
            // A sticky error would be a failure of this finding.
            Err(error) => panic!("cancellation must not be an error: {error:?}"),
        }
    }
    assert!(
        session.shared.worker_finished.load(Ordering::Acquire),
        "the worker must stop after a cancelled verification"
    );

    let stop = session.stop();
    assert!(
        stop.is_ok(),
        "a cancelled verification must stop successfully, not as a preparation error: {stop:?}"
    );
    let counters = session.shared.counters.snapshot();
    assert_eq!(counters.readback_cancelled, 1);
    assert_eq!(counters.discarded_on_cancellation, 1);
    assert_eq!(
        counters.discarded_on_verification_failure, 0,
        "cancellation is not a verification failure"
    );
    assert_eq!(session.shared.pool.counts().free, SLOT_COUNT);
    let report = session
        .diagnostics()
        .expect("diagnostics after cancellation");
    assert!(report.contains("terminal error: none"), "{report}");
    assert!(report.contains("discarded_on_cancellation=1"), "{report}");
}

/// R2 finding 2: a genuine native error is not suppressed by a set cancellation token.
#[test]
fn a_genuine_verification_error_still_fails_while_cancellation_is_requested() {
    let (entered_tx, entered_rx) = sync_channel(1);
    let (proceed_tx, proceed_rx) = sync_channel(1);
    let mut session = start_with_factory(
        synthetic_selection(),
        super::plan_owned_payload(64, 48, true).expect("synthetic plan fits"),
        CaptureContext {
            clock: Arc::new(MasterClock::default()),
            cancellation: CaptureCancellation::default(),
        },
        CaptureOptions {
            verify_pixels: true,
            hdr_float: false,
            show_cursor: true,
            defer_frames: false,
        },
        None,
        move |preparation| {
            let mut source = ScriptedSource::new(
                images(2, 1_000_000),
                ScriptPolicy {
                    query_complete: true,
                    fail_readback: true,
                    ..ScriptPolicy::default()
                },
                Arc::new(AtomicBool::new(false)),
                preparation.lifetime,
                preparation.cancellation.clone(),
            );
            source.readback_gate = Some((entered_tx, proceed_rx));
            Ok(source)
        },
    )
    .expect("the synthetic session must start");
    // Reach verification before cancellation; cancelling earlier can legitimately
    // stop acquisition before the injected failure ever runs.
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("verification must start");
    session.cancellation.cancel();
    proceed_tx.send(()).expect("verification is waiting");
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut error = None;
    while error.is_none() && Instant::now() < deadline {
        match session.try_frame() {
            Ok(Some(_)) => {}
            Ok(None) => std::thread::sleep(Duration::from_millis(1)),
            Err(observed) => error = Some(observed),
        }
    }
    let error = error.expect("a genuine verification failure must surface");
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(error.message.contains("SYNTHETIC verification failure"));
    let stop = session.stop();
    assert_eq!(
        stop.expect_err("the failure must survive stop").code,
        ErrorCode::Internal
    );
}

/// R2 finding 3: a poisoned sample log is an explicit terminal failure, not contention.
#[test]
fn a_poisoned_sample_log_is_an_explicit_terminal_failure() {
    let (shared, lifetime) = synthetic_shared_state(false);
    poison(&shared.log);
    let shared = run_script_on(
        shared,
        lifetime,
        images(2, 1_000_000),
        ScriptPolicy {
            query_complete: true,
            ..ScriptPolicy::default()
        },
        false,
    );
    let terminal = shared
        .terminal_error()
        .expect("log poison must publish a sticky terminal error");
    assert_eq!(terminal.code, ErrorCode::Internal);
    assert!(
        terminal.message.contains("poisoned"),
        "{}",
        terminal.message
    );
    let counters = shared.counters.snapshot();
    assert_eq!(
        counters.ready_published, 1,
        "the frame itself was published"
    );
    assert!(
        counters
            .identities(shared.ready.lock().unwrap().stats(), shared.pool.counts())
            .all(),
        "poison must not break the accounting"
    );
}

/// R2 finding 3: ready-queue poison is persistent through the public dequeue and stop.
#[test]
fn a_poisoned_ready_queue_is_persistent_for_dequeue_and_stop() {
    let mut session = synthetic_session(
        images(4, 1_000_000),
        ScriptPolicy {
            query_complete: true,
            ..ScriptPolicy::default()
        },
        CaptureOptions::default(),
    );
    poison(&session.shared.ready);

    let first = match session.try_frame() {
        Err(error) => error,
        Ok(_) => panic!("a poisoned ready queue cannot be dequeued"),
    };
    assert_eq!(first.code, ErrorCode::Internal);
    assert!(first.message.contains("poisoned"), "{}", first.message);
    // The error is sticky, not transient.
    let second = match session.try_frame() {
        Err(error) => error,
        Ok(_) => panic!("the poison must persist across calls"),
    };
    assert_eq!(second.code, ErrorCode::Internal);
    let stop = session
        .stop()
        .expect_err("stop reports the poison it salvaged through");
    assert_eq!(stop.code, ErrorCode::Internal);
    let report = session.diagnostics().expect("diagnostics after poison");
    assert!(report.contains("terminal error: code=Internal"), "{report}");
}

/// R2 finding 1 at the public boundary: the audit's exact scenario, where a session
/// unwinds with an incomplete copy in flight and the consumer then stops it.
#[test]
fn a_session_panic_with_a_pending_copy_leaves_no_occupied_slot() {
    let mut session = synthetic_session(
        vec![
            Step::Delay(50),
            Step::Image {
                last_present: 1_000_000,
                accumulated: 1,
            },
            Step::Panic,
        ],
        ScriptPolicy::default(),
        CaptureOptions::default(),
    );

    // The panic happens after readiness and after one slot was claimed.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut error = None;
    while error.is_none() && Instant::now() < deadline {
        match session.try_frame() {
            Ok(_) => std::thread::sleep(Duration::from_millis(1)),
            Err(observed) => error = Some(observed),
        }
    }
    let error = error.expect("the panic must surface as a sticky error");
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(error.message.contains("panicked"), "{}", error.message);

    let stop = session.stop();
    assert_eq!(
        stop.expect_err("the failure survives teardown").code,
        ErrorCode::Internal
    );
    let counters = session.shared.counters.snapshot();
    let slots = session.shared.pool.counts();
    assert_eq!(
        slots.free, SLOT_COUNT,
        "an unwinding worker must not leave a slot occupied"
    );
    assert_eq!(slots.occupied(), 0);
    assert_eq!(counters.copy_submissions, 1);
    assert_eq!(counters.shutdown_discarded_pending, 1);
    assert!(
        counters
            .identities(session.shared.ready.lock().unwrap().stats(), slots)
            .all()
    );
}
