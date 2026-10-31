//! The capture worker (FRAME-0103, ADR 0005).
//!
//! One dedicated thread owns COM initialization, the DXGI duplication and the D3D11
//! immediate context. This module holds the loop, the bounded slot/queue policy and
//! the watchdog checks; the native API calls live behind the private
//! [`CaptureSource`] seam, which is also what the synthetic unit tests drive. No
//! callback, event handler or message pump is involved.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Condvar, Mutex, TryLockError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use frame_capture::CaptureCancellation;
use frame_clock::{ClockAnchor, MasterClock};
use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus};
use frame_metrics::{BoundedQueue, OverflowPolicy, QueueStats};
use std::num::NonZeroUsize;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext};

use crate::capture::SharedState;
use crate::capture::diagnostics::{FrameSample, NativeDescription, PixelSample, ReadbackEvidence};
use crate::capture::timing::{ObservationOutcome, TimestampPolicy, align_clock};
use crate::cursor_preview_diagnostics::{self as trace, CaptureTrace, PreviewFrameTrace};
use crate::preview::{
    PREVIEW_IMAGE_MIME, PreviewConfig, PreviewCost, PreviewCounters, PreviewHub, PreviewImage,
    PreviewSlotState, PreviewSource, ThumbnailGenerator, encode_png, error_image,
};
use crate::surface::{SharedPayload, SlotClaim, SurfaceSlot};

/// Polling bound for `AcquireNextFrame` (ADR 0005: at most 5 ms, never INFINITE).
pub(crate) const ACQUIRE_TIMEOUT_MS: u32 = 5;

/// GPU-progress watchdog (ADR 0005: five seconds, checked between calls; a driver
/// call cannot be forcibly cancelled, so this is not a hard wall-clock guarantee on
/// a broken driver).
pub(crate) const WATCHDOG: Duration = Duration::from_secs(5);

/// Bounded interval between opt-in readback samples.
pub(crate) const READBACK_INTERVAL: Duration = Duration::from_millis(500);

/// Bounded number of opt-in readback samples per session.
pub(crate) const MAX_READBACK_SAMPLES: u32 = 64;

/// One acquired desktop image as DXGI described it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FrameObservation {
    pub(crate) last_present_time: i64,
    pub(crate) last_mouse_update_time: i64,
    /// A timeout fallback sampled QPC after seeing a cursor change; it is not a
    /// DXGI `LastMouseUpdateTime` event and must not advance that native sequence.
    #[serde(default)]
    pub(crate) synthetic_cursor_time: bool,
    #[serde(default)]
    pub(crate) synthetic_replay_time: bool,
    pub(crate) accumulated_frames: u32,
    pub(crate) protected_content_masked_out: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) trace: Option<CaptureTrace>,
}

impl FrameObservation {
    /// DXGI can deliver a pointer update without a desktop presentation. The
    /// acquired resource still supplies the unchanged background for that cursor.
    pub(crate) fn visual_update_time(&self) -> Option<i64> {
        let time = self.last_present_time.max(self.last_mouse_update_time);
        (time > 0).then_some(time)
    }
}

/// One opt-in readback result, keeping the row pitch, the hashed byte range and the
/// sampled quadruples as verification evidence.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReadbackSample {
    pub(crate) row_pitch: u32,
    pub(crate) bytes: usize,
    /// `width * 4`: valid pixel bytes per row, which is what the digest covers.
    pub(crate) valid_row_bytes: usize,
    /// `valid_row_bytes * height`.
    pub(crate) hash_bytes: usize,
    pub(crate) padded: bool,
    pub(crate) digest: u64,
    pub(crate) samples: [PixelSample; crate::capture::diagnostics::READBACK_PIXEL_SAMPLES],
}

/// The explicit result of one verification readback.
///
/// Cancellation is a distinct outcome from failure: a session cancelled during
/// verification stops normally and accounts for the discarded work, while a genuine
/// native error stays an error even when the cancellation token happens to be set.
#[derive(Clone, Debug)]
pub(crate) enum ReadbackOutcome {
    /// The slot was mapped, hashed and sampled.
    Verified(ReadbackSample),
    /// The worker observed cancellation between bounded map polls.
    Cancelled,
    /// A real failure: no mapping, a native error, a watchdog expiry or an invalid
    /// mapped layout.
    Failed(RecorderError),
}

/// One acquired frame plus its mapping.
pub(crate) struct Acquired<Image> {
    pub(crate) image: Image,
    pub(crate) observation: FrameObservation,
}

/// Private seam over the native duplication session.
///
/// Every method runs on the worker thread. The native implementation is the DXGI
/// source; unit tests substitute a scripted source, so slot, queue, timestamp,
/// cancellation and watchdog behaviour stay testable without a GPU.
pub(crate) trait CaptureSource {
    /// Complete source activation after downstream preparation, before acquiring
    /// the first image. Immediate sources have already completed this step.
    fn begin_frames(&mut self) -> Result<(), RecorderError> {
        Ok(())
    }
    fn set_replay_sampling(&mut self, _enabled: bool) -> Result<(), RecorderError> {
        Ok(())
    }
    /// An opaque acquired desktop resource, released exactly once through
    /// [`CaptureSource::release_frame`].
    type Image;

    /// `Ok(None)` is the documented `DXGI_ERROR_WAIT_TIMEOUT`: no update within the
    /// polling bound. It is neither an error nor a shutdown.
    fn acquire(&mut self, timeout_ms: u32) -> Result<Option<Acquired<Self::Image>>, RecorderError>;

    /// Submits the copy into `slot_index`, ends that slot's completion query and
    /// flushes the submission.
    fn begin_copy(&mut self, image: &Self::Image, slot_index: usize) -> Result<(), RecorderError>;

    /// Reports whether the slot's copy has completed. `Ok(false)` includes the
    /// binding's `S_FALSE` result, which is not completion.
    fn query_complete(&mut self, slot_index: usize) -> Result<bool, RecorderError>;

    /// Releases the acquired desktop resource exactly once.
    fn release_frame(&mut self) -> Result<(), RecorderError>;

    /// Opt-in bounded readback of a completed slot, on the worker thread.
    ///
    /// The outcome distinguishes cancellation from failure, so a normal post-readiness
    /// cancellation stops cleanly while a genuine native error still fails.
    fn readback(&mut self, slot_index: usize) -> ReadbackOutcome;

    /// Native counter ticks per second (the fixed QPF value).
    fn ticks_per_second(&self) -> u64;

    /// One native counter reading, taken between two `MasterClock` reads.
    fn sample_counter(&mut self) -> Result<i64, RecorderError>;

    /// Immutable native description recorded in diagnostics.
    fn description(&self) -> &NativeDescription;

    /// The bounded payload owner that keeps textures alive for outstanding leases.
    fn payload_owner(&self) -> SharedPayload;

    /// Include source-owned/reserved normalization and cache textures in preview
    /// admission. Synthetic/other sources retain the shared slot-plan default.
    fn preview_payload_bytes(&self) -> Option<u64> {
        None
    }

    /// Explicit native teardown after worker accounting is closed. The default source
    /// has infallible RAII cleanup; WGC reports subscription and Close failures here.
    fn finish(&mut self) -> Result<(), RecorderError> {
        Ok(())
    }

    /// One texture of this session's own capture payload, for the preview readback
    /// (FRAME-0208).
    ///
    /// The default is `None`: a source that cannot expose its own texture gets no
    /// preview, and the worker publishes that as a preview failure instead of opening a
    /// second capture or a second device.
    fn preview_source(&self, _slot_index: usize) -> Option<PreviewSource> {
        None
    }

    /// The session's own D3D11 device, for the preview generator.
    fn preview_device(&self) -> Option<ID3D11Device> {
        None
    }

    /// The session's own immediate context, for the preview readback.
    fn preview_context(&self) -> Option<ID3D11DeviceContext> {
        None
    }
}

/// Everything the worker needs from its session.
pub(crate) struct WorkerContext {
    pub(crate) shared: Arc<SharedState>,
    pub(crate) clock: Arc<MasterClock>,
    pub(crate) cancellation: CaptureCancellation,
    pub(crate) verify_pixels: bool,
    /// The preview request this session was started with (FRAME-0208). `None` means the
    /// worker performs no preview work at all, regardless of what is attached later.
    pub(crate) preview: Option<PreviewConfig>,
}

struct PendingCopy {
    desktop_presentation: bool,
    slot: Arc<SurfaceSlot>,
    timestamp: frame_clock::MediaTimestamp,
    native_ticks: i64,
    accumulated_frames: u32,
    started: Instant,
    trace: Option<CaptureTrace>,
}

/// The worker's preview state (FRAME-0208).
///
/// It exists only when the session was started with a preview request. The generator is
/// CPU generator remains lazy for idle preview. GPU handoff storage is preflighted
/// and allocated before readiness; the live frame loop never allocates it.
struct PreviewRuntime {
    gpu_image: Result<Arc<crate::gpu_preview::SharedImage>, String>,
    generator: Option<ThumbnailGenerator>,
    hub: Option<Arc<PreviewHub>>,
    counters: Arc<PreviewCounters>,
    /// The session's own device and immediate context, as the capture source exposes
    /// them. They are used only on this worker thread.
    device: Option<ID3D11Device>,
    context: Option<ID3D11DeviceContext>,
    capture_payload_bytes: u64,
    last_queued: Option<Instant>,
    encoder: Option<PreviewEncoder>,
}

/// Only BGRA pixels and metadata cross this boundary. D3D objects stay on the
/// capture thread. The mutex protects one replaceable pending job; the worker may
/// own one other job while encoding it.
struct PreviewEncodeJob {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    media_ns: Option<u64>,
    readback_nanos: u64,
    hub: Arc<PreviewHub>,
    generation: u64,
    subscription_epoch: u64,
    epoch: u64,
}

#[derive(Default)]
struct PreviewEncodeQueue {
    pending: Option<PreviewEncodeJob>,
    closed: bool,
}

struct PreviewEncoder {
    queue: Arc<(Mutex<PreviewEncodeQueue>, Condvar)>,
    epoch: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl PreviewEncoder {
    fn start(shared: Arc<SharedState>) -> std::io::Result<Self> {
        let queue = Arc::new((Mutex::new(PreviewEncodeQueue::default()), Condvar::new()));
        let epoch = Arc::new(AtomicU64::new(1));
        let worker_queue = Arc::clone(&queue);
        let worker_epoch = Arc::clone(&epoch);
        let thread = thread::Builder::new()
            .name("frame-preview-png".into())
            .spawn(move || Self::run(worker_queue, worker_epoch, shared))?;
        Ok(Self {
            queue,
            epoch,
            thread: Some(thread),
        })
    }

    fn run(
        queue: Arc<(Mutex<PreviewEncodeQueue>, Condvar)>,
        epoch: Arc<AtomicU64>,
        shared: Arc<SharedState>,
    ) {
        loop {
            let job = {
                let (slot, ready) = &*queue;
                let mut state = slot.lock().unwrap_or_else(|poison| poison.into_inner());
                while state.pending.is_none() && !state.closed {
                    state = ready
                        .wait(state)
                        .unwrap_or_else(|poison| poison.into_inner());
                }
                if state.closed {
                    return;
                }
                state.pending.take().expect("the pending job was checked")
            };
            if job.epoch != epoch.load(Ordering::Acquire)
                || job.hub.generation() != job.generation
                || job.hub.subscription_epoch() != job.subscription_epoch
                || !job.hub.enabled()
            {
                continue;
            }
            let started = Instant::now();
            let mut encoded = Vec::with_capacity(96 * 1024);
            let result = encode_png(&job.pixels, job.width, job.height, &mut encoded);
            let encode_nanos = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
            if job.epoch != epoch.load(Ordering::Acquire)
                || job.hub.generation() != job.generation
                || job.hub.subscription_epoch() != job.subscription_epoch
                || !job.hub.enabled()
            {
                continue;
            }
            let counters = job.hub.counters_handle();
            counters
                .encode_nanos
                .fetch_add(encode_nanos, Ordering::Relaxed);
            match result {
                Ok(()) => {
                    counters.generated.fetch_add(1, Ordering::Relaxed);
                    let cost = PreviewCost {
                        readback_nanos: job.readback_nanos,
                        encode_nanos,
                        image_width: job.width,
                        image_height: job.height,
                        encoded_bytes: encoded.len(),
                    };
                    if job
                        .hub
                        .publish(PreviewImage {
                            width: job.width,
                            height: job.height,
                            sequence: 0,
                            first_of_session: false,
                            source_media_ns: job.media_ns,
                            session_generation: job.generation,
                            mime_type: PREVIEW_IMAGE_MIME,
                            data: Ok(encoded),
                        })
                        .is_some()
                        && let Ok(mut slot) = shared.preview_cost.try_lock()
                    {
                        slot.replace(PreviewSlotState {
                            cost,
                            media_nanos: job.media_ns,
                        });
                    }
                }
                Err(error) => job.hub.publish_error(error_image(
                    job.width,
                    job.height,
                    job.media_ns,
                    error.message,
                )),
            }
        }
    }

    fn invalidate(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        if let Ok(mut state) = self.queue.0.try_lock() {
            state.pending = None;
        }
    }

    /// Takes the replaceable buffer before mapping another frame, keeping at most
    /// two raw images alive: one encoding and one on the capture thread or queued.
    fn take_pending_buffer(&self) -> Result<Option<Vec<u8>>, ()> {
        match self.queue.0.try_lock() {
            Ok(mut state) => Ok(state.pending.take().map(|job| job.pixels)),
            Err(_) => Err(()),
        }
    }

    fn submit(&self, mut job: PreviewEncodeJob) {
        job.epoch = self.epoch.load(Ordering::Acquire);
        if let Ok(mut state) = self.queue.0.try_lock()
            && !state.closed
        {
            state.pending = Some(job);
            self.queue.1.notify_one();
        }
    }
}

impl Drop for PreviewEncoder {
    fn drop(&mut self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        let mut state = self
            .queue
            .0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.pending = None;
        state.closed = true;
        drop(state);
        self.queue.1.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl PreviewRuntime {
    fn new(
        counters: Arc<PreviewCounters>,
        device: Option<ID3D11Device>,
        context: Option<ID3D11DeviceContext>,
        capture_payload_bytes: u64,
        width: u32,
        height: u32,
    ) -> Self {
        let gpu_image = device
            .as_ref()
            .ok_or_else(|| "GPU preview device is unavailable.".to_string())
            .and_then(|device| {
                crate::gpu_preview::prepare_shared(device, width, height, capture_payload_bytes)
            });
        Self {
            gpu_image,
            generator: None,
            hub: None,
            counters,
            device,
            context,
            capture_payload_bytes,
            last_queued: None,
            encoder: None,
        }
    }

    /// Ensures the generator exists for this session's own device and context.
    fn ensure_generator(&mut self, config: PreviewConfig) -> bool {
        if self.generator.is_some() {
            return true;
        }
        let device = self.device.clone();
        let context = self.context.clone();
        // The generator bumps the same counters the hub reports, so "generated", the worker
        // cost and everything the consumer sees describe one and the same run. The hub is
        // always attached here: without one no generation is attempted at all.
        let counters = match self.hub.as_ref() {
            Some(hub) => Arc::clone(hub.counters_handle()),
            None => Arc::clone(&self.counters),
        };
        match ThumbnailGenerator::create(
            device,
            context,
            config,
            self.capture_payload_bytes,
            &counters,
        ) {
            Ok(generator) => {
                self.generator = Some(generator);
                true
            }
            Err(error) => {
                self.fail_now(error.message(), None);
                false
            }
        }
    }

    /// Adopts an attached hub.
    ///
    /// The worker deliberately does **not** subscribe: the only subscribers are real
    /// consumers, so [`PreviewHub::enabled`] answers "is anybody watching" exactly, and
    /// the last consumer detaching stops every GPU copy, map, encode and allocation on
    /// the next completed surface. Adopting a hub is also where the generator is created
    /// for the first time, so a session whose preview is never consumed allocates no
    /// preview texture at all.
    fn refresh(&mut self, shared: &Arc<SharedState>, config: PreviewConfig) -> bool {
        let hub = match shared.preview.try_lock() {
            Ok(slot) => slot.clone(),
            // A contended preview slot only skips this step: the worker never waits for
            // a consumer, and the next completed surface retries it.
            Err(TryLockError::WouldBlock) => return false,
            Err(TryLockError::Poisoned(_)) => return false,
        };
        let Some(hub) = hub else {
            if self.hub.take().is_some() {
                if let Some(generator) = self.generator.as_mut() {
                    generator.discard_pending();
                }
                if let Some(encoder) = self.encoder.as_ref() {
                    encoder.invalidate();
                }
            }
            return false;
        };
        let known = self
            .hub
            .as_ref()
            .is_some_and(|attached| Arc::ptr_eq(attached, &hub));
        if !known {
            if let Some(generator) = self.generator.as_mut() {
                generator.discard_pending();
            }
            if let Some(encoder) = self.encoder.as_ref() {
                encoder.invalidate();
            }
            let gpu_attached = hub.gpu_sink().is_some();
            self.hub = Some(hub);
            if self.generator.is_none()
                && !gpu_attached
                && self
                    .hub
                    .as_ref()
                    .is_some_and(|hub| hub.cpu_preview_allowed())
            {
                // A rejected request is published as an error image so the UI can show
                // why, and the recording continues unaffected.
                let _ = self.ensure_generator(config);
            }
        }
        true
    }

    /// Publishes one failure without a capture surface behind it.
    fn fail_now(&self, message: String, media_ns: Option<u64>) {
        let (Some(hub), Some(generator)) = (self.hub.as_ref(), self.generator.as_ref()) else {
            return;
        };
        let config = generator.config();
        hub.publish_error(error_image(
            config.max_width,
            config.max_height,
            media_ns,
            message,
        ));
    }
}

struct Runtime {
    shared: Arc<SharedState>,
    cancellation: CaptureCancellation,
    verify_pixels: bool,
    anchor: ClockAnchor,
    policy: TimestampPolicy,
    /// Native presentation identity is distinct from pointer/visual delivery and
    /// is observed even when pressure prevents copying the new desktop image.
    last_desktop_presentation: Option<i64>,
    pending: Vec<PendingCopy>,
    sequence: u64,
    readback_samples: u32,
    last_readback: Option<Instant>,
    last_digest: Option<u64>,
    /// `Some` only when this session was started with a preview request.
    preview: Option<PreviewRuntime>,
    /// The runtime mirror of the session's preview request, read once at start.
    preview_config: Option<PreviewConfig>,
}

/// Runs one capture session's worker loop to completion.
///
/// The worker reports readiness exactly once through `ready`, handing the session its
/// own strong reference to the Frame-owned payload. Every exit path discards in-flight
/// work with accounting and leaves the ready queue for the session to drain after the
/// join, so neither side counts a frame twice. A panicking worker is turned into a
/// sticky error by [`super::start_with_factory`], not by this function.
pub(crate) fn run<S: CaptureSource>(
    mut source: S,
    context: WorkerContext,
    ready: SyncSender<Result<SharedPayload, RecorderError>>,
) {
    let WorkerContext {
        shared,
        clock,
        cancellation,
        verify_pixels,
        preview: preview_config,
    } = context;

    // A poison in a session lock is an explicit Internal failure, never a silent
    // recovery of the guard.
    let publish_description = match shared.native.lock() {
        Ok(mut native) => {
            native.replace(source.description().clone());
            Ok(())
        }
        Err(_) => Err(super::poisoned_error()),
    };
    if let Err(error) = publish_description {
        shared.fail(error.clone());
        shared.mark_worker_finished();
        let _ = ready.send(Err(error));
        return;
    }

    let ticks_per_second = source.ticks_per_second();
    let alignment = align_clock(&clock, &cancellation, ticks_per_second, || {
        source.sample_counter()
    });
    let anchor = match alignment {
        Ok((anchor, evidence)) => {
            let stored = match shared.clock_evidence.lock() {
                Ok(mut slot) => {
                    slot.replace(evidence);
                    Ok(())
                }
                Err(_) => Err(super::poisoned_error()),
            };
            match stored {
                Ok(()) => anchor,
                Err(error) => {
                    shared.fail(error.clone());
                    shared.mark_worker_finished();
                    let _ = ready.send(Err(error));
                    return;
                }
            }
        }
        Err(error) => {
            shared.fail(error.clone());
            shared.mark_worker_finished();
            let _ = ready.send(Err(error));
            return;
        }
    };

    // The preview generator is built from the session's own payload owner: the same
    // device the capture path copied into, and the same immediate context the worker
    // already owns. Nothing is created here unless a preview was requested.
    let preview_device = source.preview_device();
    let preview_context = source.preview_context();

    let mut runtime = Runtime {
        shared: shared.clone(),
        cancellation: cancellation.clone(),
        verify_pixels,
        anchor,
        policy: TimestampPolicy::new(),
        last_desktop_presentation: None,
        pending: Vec::new(),
        sequence: 0,
        readback_samples: 0,
        last_readback: None,
        last_digest: None,
        preview: preview_config.map(|_| {
            PreviewRuntime::new(
                shared.preview_counters_handle(),
                preview_device,
                preview_context,
                source
                    .preview_payload_bytes()
                    .unwrap_or(shared.payload_plan.total_bytes),
                shared.width(),
                shared.height(),
            )
        }),
        preview_config,
    };

    // Hand the session its own strong payload reference before announcing readiness, so
    // a frame can never be dequeued before a lease can be built.
    if ready.send(Ok(source.payload_owner())).is_err() {
        // The control side gave up; nothing is left running.
        shared.mark_worker_finished();
        return;
    }

    // No first image is taken and held while NVENC/mux setup uses this device.
    // This is a startup handshake only; the live nonblocking loop is unchanged.
    while !shared.acquisition_ready.load(Ordering::Acquire) {
        if cancellation.is_cancelled() {
            if let Err(error) = source.finish() {
                shared.fail(error);
            }
            shared.mark_worker_finished();
            return;
        }
        thread::park_timeout(super::READINESS_POLL);
    }
    if let Err(error) = source.begin_frames() {
        shared.fail(error);
        if let Err(error) = source.finish() {
            shared.fail(error);
        }
        shared.mark_worker_finished();
        return;
    }
    match shared.native.lock() {
        Ok(mut native) => {
            native.replace(source.description().clone());
        }
        Err(_) => {
            shared.fail(super::poisoned_error());
            shared.mark_worker_finished();
            return;
        }
    }

    loop {
        if runtime.cancellation.is_cancelled() {
            break;
        }
        if let Some(error) = runtime.watchdog_expired() {
            runtime.fail(error);
            break;
        }
        match runtime.poll_pending(&mut source) {
            // A cancelled verification is a normal stop, not a failure.
            Ok(PollProgress::Cancelled) => break,
            Ok(PollProgress::Continue) => {}
            Err(error) => {
                runtime.fail(error);
                break;
            }
        }
        // A queued staging copy can complete on a static desktop. This is one
        // non-blocking attempt in the existing acquisition loop, not a new worker.
        runtime.poll_preview_completion();
        // A copy awaiting its event must not sit behind another desktop frame
        // (or a 5 ms timeout). Poll acquisition without waiting while GPU work is
        // owned; a genuinely idle source retains the bounded blocking acquire.
        let timeout = acquisition_timeout(!runtime.pending.is_empty());
        if let Err(error) =
            source.set_replay_sampling(runtime.shared.replay_sampling.load(Ordering::Acquire))
        {
            runtime.fail(error);
            break;
        }
        match source.acquire(timeout) {
            Ok(Some(acquired)) => {
                SharedState::bump(&runtime.shared.counters.acquire_success);
                let processed = runtime.process(&mut source, &acquired);
                // Exactly-once release on every path after a successful acquire, with
                // the processing failure kept as the primary error.
                let released = source.release_frame();
                let primary = processed.err();
                match (primary, released) {
                    (primary, Err(error)) => {
                        SharedState::bump(&runtime.shared.counters.native_failures);
                        runtime.fail(merge_release_failure(primary, error));
                        break;
                    }
                    (Some(error), Ok(())) => {
                        runtime.fail(error);
                        break;
                    }
                    (None, Ok(())) => {}
                }
            }
            Ok(None) => {
                SharedState::bump(&runtime.shared.counters.acquire_timeouts);
                if timeout == 0 {
                    thread::yield_now();
                }
            }
            Err(error) => {
                SharedState::bump(&runtime.shared.counters.acquire_failures);
                runtime.fail(error);
                break;
            }
        }
    }

    runtime.shutdown();
    if let Err(error) = source.finish() {
        shared.fail(error);
    }
    shared.mark_worker_finished();
}

fn acquisition_timeout(copy_pending: bool) -> u32 {
    if copy_pending { 0 } else { ACQUIRE_TIMEOUT_MS }
}

/// Whether the pending-copy step should keep running or stop normally.
pub(crate) enum PollProgress {
    Continue,
    /// Verification observed cancellation: stop the worker cleanly.
    Cancelled,
}

impl Runtime {
    fn watchdog_expired(&self) -> Option<RecorderError> {
        let oldest = self.pending.first()?;
        let elapsed = oldest.started.elapsed();
        (elapsed > WATCHDOG).then(|| {
            SharedState::bump(&self.shared.counters.native_failures);
            internal_error(format!(
                "A GPU copy submitted {} ms ago had still not completed, past the {} ms GPU-progress watchdog.",
                elapsed.as_millis(),
                WATCHDOG.as_millis()
            ))
        })
    }

    fn fail(&self, error: RecorderError) {
        self.shared.fail(error);
    }

    fn process<S: CaptureSource>(
        &mut self,
        source: &mut S,
        acquired: &Acquired<S::Image>,
    ) -> Result<(), RecorderError> {
        let observation = acquired.observation;
        let Some(observed_time) = observation.visual_update_time() else {
            trace::record(
                "capture_drop",
                observation.trace,
                || serde_json::json!({"reason": "no_visual_timestamp"}),
            );
            SharedState::bump(&self.shared.counters.pointer_only);
            return Ok(());
        };
        let mut desktop_presentation = false;
        if observation.last_present_time > 0 {
            SharedState::bump(&self.shared.counters.updates_with_image);
            let ticks = observation.last_present_time;
            if self
                .last_desktop_presentation
                .is_none_or(|previous| ticks > previous)
            {
                self.last_desktop_presentation = Some(ticks);
                if self.anchor.map(ticks as u64).is_some() {
                    desktop_presentation = true;
                    SharedState::bump(&self.shared.counters.native_desktop_presentations);
                }
            }
        } else if observation.synthetic_replay_time {
            SharedState::bump(&self.shared.counters.replay_samples);
        } else {
            SharedState::bump(&self.shared.counters.pointer_only);
        }
        if observation.protected_content_masked_out {
            SharedState::bump(&self.shared.counters.protected_content);
            self.shared
                .counters
                .protected_content_seen
                .store(true, Ordering::Relaxed);
        }
        if observation.accumulated_frames > 1 {
            let missed = u64::from(observation.accumulated_frames - 1);
            self.shared
                .counters
                .upstream_accumulated_updates
                .fetch_add(missed, Ordering::Relaxed);
        }

        let outcome = if observation.synthetic_cursor_time || observation.synthetic_replay_time {
            self.policy
                .observe_synthetic_cursor(&self.anchor, observation.last_mouse_update_time)
        } else {
            self.policy.observe(
                &self.anchor,
                observation.last_present_time,
                observation.last_mouse_update_time,
            )
        };
        match outcome {
            ObservationOutcome::Repeat => {
                trace::record(
                    "capture_drop",
                    observation.trace,
                    || serde_json::json!({"reason": "repeat"}),
                );
                SharedState::bump(&self.shared.counters.repeats);
                Ok(())
            }
            ObservationOutcome::StaleVisual => {
                trace::record(
                    "capture_drop",
                    observation.trace,
                    || serde_json::json!({"reason": "stale_visual"}),
                );
                SharedState::bump(&self.shared.counters.stale_visual);
                Ok(())
            }
            ObservationOutcome::PreEpoch => {
                trace::record(
                    "capture_drop",
                    observation.trace,
                    || serde_json::json!({"reason": "pre_epoch"}),
                );
                SharedState::bump(&self.shared.counters.pre_epoch);
                Ok(())
            }
            ObservationOutcome::Discontinuity { previous, observed } => {
                SharedState::bump(&self.shared.counters.discontinuities);
                Err(source_lost_error(format!(
                    "The desktop presentation timestamp moved backwards from {previous} to {observed} QPC ticks, so the captured desktop image sequence is not continuous."
                )))
            }
            ObservationOutcome::PointerDiscontinuity { previous, observed } => {
                SharedState::bump(&self.shared.counters.discontinuities);
                Err(source_lost_error(format!(
                    "The desktop pointer timestamp moved backwards from {previous} to {observed} QPC ticks, so the captured pointer sequence is not continuous."
                )))
            }
            ObservationOutcome::Unmappable { observed } => {
                SharedState::bump(&self.shared.counters.unmappable);
                Err(internal_error(format!(
                    "The desktop presentation timestamp {observed} QPC ticks could not be placed on the injected session timeline."
                )))
            }
            ObservationOutcome::Accepted(timestamp) => {
                // The claim counts the copy attempt and reclaims the slot if the worker
                // unwinds before the copy is handed to the pending list.
                let Some(claim) = SlotClaim::for_copy(&self.shared.pool, &self.shared.counters)
                else {
                    // All four slots are occupied: drop this update rather than
                    // allocating a fifth surface.
                    SharedState::bump(&self.shared.counters.pool_exhausted);
                    trace::record(
                        "capture_drop",
                        observation.trace,
                        || serde_json::json!({"reason": "pool_exhausted"}),
                    );
                    return Ok(());
                };
                if let Err(error) = source.begin_copy(&acquired.image, claim.index()) {
                    claim.fail_copy();
                    return Err(error);
                }
                self.pending.push(PendingCopy {
                    desktop_presentation,
                    slot: claim.into_slot(),
                    timestamp,
                    native_ticks: observed_time,
                    accumulated_frames: observation.accumulated_frames,
                    started: Instant::now(),
                    trace: observation.trace,
                });
                Ok(())
            }
        }
    }

    fn poll_pending<S: CaptureSource>(
        &mut self,
        source: &mut S,
    ) -> Result<PollProgress, RecorderError> {
        // Completion is consumed in submission order, so published timestamps stay in
        // the native presentation order.
        while let Some(front) = self.pending.first() {
            let slot_index = front.slot.index();
            if !source.query_complete(slot_index)? {
                break;
            }
            // The completed copy is taken into explicit scoped ownership before any
            // fallible step, so no path — including an unwind — can strand its slot.
            let copy = self.pending.remove(0);
            SharedState::bump(&self.shared.counters.copy_completions);
            if copy.desktop_presentation {
                SharedState::bump(&self.shared.counters.captured_desktop_presentations);
            }
            let claim = SlotClaim::for_completed(copy.slot.clone(), &self.shared.counters);
            let media_ns = Some(copy.timestamp.0);
            let frame_trace = copy.trace.map(|capture| PreviewFrameTrace {
                capture,
                native_ticks: copy.native_ticks,
                media_ns,
                completed_ns: trace::now_ns(),
            });
            trace::record("capture_complete", copy.trace, || {
                let counters = self.shared.counters.snapshot();
                serde_json::json!({
                    "slot": slot_index, "native_ticks": copy.native_ticks, "media_ns": media_ns,
                    "desktop_presentation": copy.desktop_presentation,
                    "copy_completion_ns": copy.started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
                    "counters": { "acquire_success": counters.acquire_success,
                        "copy_completions": counters.copy_completions,
                        "native_desktop_presentations": counters.native_desktop_presentations,
                        "captured_desktop_presentations": counters.captured_desktop_presentations,
                        "pool_exhausted": counters.pool_exhausted,
                        "ready_evictions": counters.ready_evictions,
                        "worker_contention_drops": counters.worker_contention_drops,
                        "upstream_accumulated_updates": counters.upstream_accumulated_updates }
                })
            });
            match self.verify_slot(source, slot_index) {
                Ok(VerificationStep::Ready(verified_changed)) => {
                    // Queue any admitted GPU preview copy while this slot is
                    // still exclusively owned. Publication lets the encoder
                    // release/recycle it, so preview must not read it afterwards.
                    self.maybe_generate_preview(source, slot_index, media_ns, frame_trace);
                    self.publish(claim, copy, verified_changed)?
                }
                Ok(VerificationStep::Cancelled) => {
                    // Cancellation during verification is a normal stop: the slot is
                    // returned and the discarded verification is counted.
                    claim.fail_cancellation();
                    return Ok(PollProgress::Cancelled);
                }
                Err(error) => {
                    claim.fail_verification();
                    return Err(error);
                }
            }
        }
        Ok(PollProgress::Continue)
    }

    /// Produces at most one bounded preview image for a completed slot.
    ///
    /// Every gate is checked before any work: the session must have been started with a
    /// preview request, a hub must be attached, at least one subscriber must be
    /// attached, and the transport's bound must have elapsed (independent GPU
    /// refresh cap or PNG 10 FPS cap). Hidden/minimized GPU windows suspend
    /// submission. A preview problem is never returned as a capture failure.
    fn maybe_generate_preview<S: CaptureSource>(
        &mut self,
        source: &mut S,
        slot_index: usize,
        media_ns: Option<u64>,
        frame_trace: Option<PreviewFrameTrace>,
    ) {
        let Some(config) = self.preview_config else {
            return;
        };
        let Some(preview) = self.preview.as_mut() else {
            return;
        };
        if !preview.refresh(&self.shared, config) {
            return;
        }
        let Some(hub) = preview.hub.clone() else {
            return;
        };
        if let Some(gpu) = hub.gpu_sink() {
            if !gpu.active() {
                trace::record(
                    "preview_skip",
                    frame_trace.map(|frame| frame.capture),
                    || serde_json::json!({"reason": "inactive"}),
                );
                return;
            }
            if let Some(generator) = preview.generator.as_mut() {
                generator.discard_pending();
            }
            if let Some(encoder) = preview.encoder.as_ref() {
                encoder.invalidate();
            }
            // Gate before preview_source and all D3D work. Admission never waits
            // for the UI and consumes the interval even if the shared slot is
            // busy; there is one replaceable latest image, never a backlog.
            if !gpu.admit_frame() {
                trace::record(
                    "preview_skip",
                    frame_trace.map(|frame| frame.capture),
                    || {
                        let counters = gpu.stats();
                        serde_json::json!({
                            "reason": "admission", "counters": { "frames_considered": counters.frames_considered,
                                "rate_limited": counters.rate_limited, "suspended": counters.suspended,
                                "producer_busy": counters.producer_busy, "presenter_busy": counters.presenter_busy,
                                "copied": counters.copied, "presented": counters.presented }
                        })
                    },
                );
                return;
            }
            let (Some(device), Some(context), Some(preview_source)) = (
                preview.device.as_ref(),
                preview.context.as_ref(),
                source.preview_source(slot_index),
            ) else {
                gpu.fail("This capture session has no D3D11 GPU preview source.".into());
                return;
            };
            match &preview.gpu_image {
                Ok(image) if gpu.prepare_image(image) => {}
                Ok(_) => return,
                Err(error) => {
                    gpu.fail(error.clone());
                    return;
                }
            }
            if let Err(error) = gpu.submit_with_frame(
                &preview_source,
                device,
                context,
                preview.capture_payload_bytes,
                frame_trace,
            ) {
                gpu.fail(error);
            }
            return;
        }
        if !hub.cpu_preview_allowed() {
            return;
        }
        if !hub.enabled() {
            // No subscriber: no GPU copy, no map, no encode and no allocation.
            hub.note_skipped();
            return;
        }
        if let Some(last) = preview.last_queued
            && last.elapsed() < config.min_interval
        {
            hub.note_rate_limited();
            return;
        }
        if !preview.ensure_generator(config) {
            return;
        }
        let Some(preview_source) = source.preview_source(slot_index) else {
            hub.publish_error(error_image(
                config.max_width,
                config.max_height,
                media_ns,
                "The capture session does not expose a preview source for this surface, so no preview image was produced."
                    .to_string(),
            ));
            return;
        };
        let Some(generator) = preview.generator.as_mut() else {
            return;
        };
        match generator.queue(preview_source, media_ns) {
            Ok(true) => preview.last_queued = Some(Instant::now()),
            Ok(false) => {} // The previous readback still owns the single staging slot.
            Err(error) => preview.fail_now(error.message(), media_ns),
        }
    }

    /// Finishes a staged preview even if no new desktop image arrives. A detached
    /// subscriber causes only a pending-state discard: no map or encode is attempted.
    fn poll_preview_completion(&mut self) {
        let (Some(config), Some(preview)) = (self.preview_config, self.preview.as_mut()) else {
            return;
        };
        if !preview.refresh(&self.shared, config) {
            return;
        }
        let Some(hub) = preview.hub.as_ref() else {
            return;
        };
        if !hub.cpu_preview_allowed() || hub.gpu_sink().is_some() {
            if let Some(generator) = preview.generator.as_mut() {
                generator.discard_pending();
            }
            if let Some(encoder) = preview.encoder.as_ref() {
                encoder.invalidate();
            }
            return;
        }
        let Some(generator) = preview.generator.as_mut() else {
            return;
        };
        if !hub.enabled() {
            generator.discard_pending();
            if let Some(encoder) = preview.encoder.as_ref() {
                encoder.invalidate();
            }
            return;
        }
        if !generator.has_pending() {
            return;
        }
        if preview.encoder.is_none() {
            match PreviewEncoder::start(Arc::clone(&self.shared)) {
                Ok(encoder) => preview.encoder = Some(encoder),
                Err(error) => {
                    generator.discard_pending();
                    preview.fail_now(
                        format!("The preview encoder thread could not start: {error}"),
                        None,
                    );
                    return;
                }
            }
        }
        let Some(encoder) = preview.encoder.as_ref() else {
            return;
        };
        let mut pixels = match encoder.take_pending_buffer() {
            Ok(Some(buffer)) => buffer,
            Ok(None) => Vec::new(),
            Err(()) => return, // A brief queue-lock collision never waits on capture.
        };
        let outcome = generator.readback_pending(&mut pixels);
        match outcome {
            Ok(Some((width, height, media_ns, readback_nanos))) => {
                encoder.submit(PreviewEncodeJob {
                    pixels,
                    width,
                    height,
                    media_ns,
                    readback_nanos,
                    hub: Arc::clone(hub),
                    generation: hub.generation(),
                    subscription_epoch: hub.subscription_epoch(),
                    epoch: 0,
                });
            }
            Ok(None) => {}
            Err(error) => {
                generator.discard_pending();
                preview.fail_now(error.message(), None);
            }
        }
    }

    /// Runs the opt-in bounded readback for one completed slot. Ordinary capture
    /// never reaches this path.
    fn verify_slot<S: CaptureSource>(
        &mut self,
        source: &mut S,
        slot_index: usize,
    ) -> Result<VerificationStep, RecorderError> {
        if !self.verify_pixels || self.readback_samples >= MAX_READBACK_SAMPLES {
            return Ok(VerificationStep::Ready(None));
        }
        if self
            .last_readback
            .is_some_and(|last| last.elapsed() < READBACK_INTERVAL)
        {
            return Ok(VerificationStep::Ready(None));
        }
        self.last_readback = Some(Instant::now());
        self.readback_samples += 1;
        let sample = match source.readback(slot_index) {
            ReadbackOutcome::Verified(sample) => sample,
            ReadbackOutcome::Cancelled => {
                SharedState::bump(&self.shared.counters.readback_cancelled);
                return Ok(VerificationStep::Cancelled);
            }
            ReadbackOutcome::Failed(error) => {
                // A genuine native error stays an error, even if the cancellation token
                // happens to be set.
                SharedState::bump(&self.shared.counters.readback_failures);
                return Err(error);
            }
        };
        SharedState::bump(&self.shared.counters.readback_copies);
        let evidence = ReadbackEvidence {
            row_pitch: sample.row_pitch,
            bytes: sample.bytes,
            valid_row_bytes: sample.valid_row_bytes,
            hash_bytes: sample.hash_bytes,
            padded: sample.padded,
            samples: sample.samples,
        };
        // The evidence snapshot uses try_lock like the sample log: the worker never
        // waits for diagnostics, contention only skips the snapshot, and poison is an
        // explicit failure.
        match self.shared.readback.try_lock() {
            Ok(mut slot) => {
                slot.replace(evidence);
            }
            Err(TryLockError::WouldBlock) => {
                SharedState::bump(&self.shared.counters.readback_evidence_skipped);
            }
            Err(TryLockError::Poisoned(_)) => {
                SharedState::bump(&self.shared.counters.readback_failures);
                return Err(super::poisoned_error());
            }
        }
        let changed = self.last_digest.map(|previous| previous != sample.digest);
        if changed == Some(true) {
            SharedState::bump(&self.shared.counters.readback_changed);
        }
        self.last_digest = Some(sample.digest);
        Ok(VerificationStep::Ready(changed))
    }

    fn publish(
        &mut self,
        claim: SlotClaim,
        copy: PendingCopy,
        verified_changed: Option<bool>,
    ) -> Result<(), RecorderError> {
        claim.slot().mark_ready();
        let frame = ReadyFrame {
            slot: Arc::clone(claim.slot()),
            timestamp: copy.timestamp,
            width: self.shared.width(),
            height: self.shared.height(),
        };
        // The ready-queue guard is released before anything else is touched, so no
        // other lock is ever acquired while holding it.
        let outcome = match self.shared.ready.try_lock() {
            Ok(mut queue) => PublishOutcome::Pushed(queue.push(frame)),
            Err(TryLockError::WouldBlock) => PublishOutcome::Contended,
            Err(TryLockError::Poisoned(_)) => PublishOutcome::Poisoned,
        };
        match outcome {
            PublishOutcome::Pushed(evicted) => {
                trace::record("recording_publish", copy.trace, || {
                    serde_json::json!({
                        "native_ticks": copy.native_ticks, "media_ns": copy.timestamp.0,
                        "evicted_previous": evicted.is_some()
                    })
                });
                // The frame is in the queue: ownership has left the guard.
                let _published = claim.into_slot();
                if let Some(evicted) = evicted {
                    evicted.slot.mark_free();
                    SharedState::bump(&self.shared.counters.ready_evictions);
                }
                SharedState::bump(&self.shared.counters.ready_published);
                self.shared.notify_owner();
                self.sequence += 1;
                // Diagnostic samples are recorded with try_lock only: the worker never
                // waits for diagnostics. Contention skips a sample; poison is an
                // explicit failure rather than a silent skip.
                self.record_sample(FrameSample {
                    sequence: self.sequence,
                    native_ticks: copy.native_ticks,
                    media_nanos: copy.timestamp.0,
                    accumulated_frames: copy.accumulated_frames,
                    verified_content_changed: verified_changed,
                })
            }
            PublishOutcome::Contended => {
                trace::record(
                    "recording_drop",
                    copy.trace,
                    || serde_json::json!({"reason": "ready_queue_contention"}),
                );
                // Short try_lock only: a consumer holding the queue briefly must not
                // stall the worker, so this completed frame is dropped and counted.
                claim.fail_contention();
                Ok(())
            }
            PublishOutcome::Poisoned => {
                claim.fail_queue_poison();
                Err(super::poisoned_error())
            }
        }
    }

    fn record_sample(&mut self, sample: FrameSample) -> Result<(), RecorderError> {
        match self.shared.log.try_lock() {
            Ok(mut log) => {
                log.record(sample);
                Ok(())
            }
            Err(TryLockError::WouldBlock) => {
                // Contention may skip a diagnostic sample.
                SharedState::bump(&self.shared.counters.diagnostic_samples_skipped);
                Ok(())
            }
            Err(TryLockError::Poisoned(_)) => {
                // Poison is not contention: it terminates the worker explicitly.
                SharedState::bump(&self.shared.counters.diagnostic_samples_skipped);
                Err(super::poisoned_error())
            }
        }
    }

    /// Discards in-flight copies with accounting.
    ///
    /// Called explicitly on the normal exit path and again from `Drop`, so a worker
    /// that unwinds past this point still reclaims every slot it owns. The second call
    /// finds an empty list, so nothing is counted twice.
    fn shutdown(&mut self) {
        if let Some(preview) = self.preview.as_mut() {
            // Completion is reported only after the CPU encoder has stopped.
            preview.encoder.take();
        }
        for pending in self.pending.drain(..) {
            pending.slot.mark_free();
            SharedState::bump(&self.shared.counters.shutdown_discarded_pending);
        }
    }
}

impl Drop for Runtime {
    /// Unwind safety net for owned in-flight copies (FRAME-0103-R2 finding 1).
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The result of one verification step.
enum VerificationStep {
    /// Continue, with `Some(changed)` when a readback ran.
    Ready(Option<bool>),
    /// The session was cancelled during verification: stop normally.
    Cancelled,
}

/// The outcome of one ready-queue push attempt.
///
/// Modelling the attempt as a value lets the queue guard be released before any other
/// lock is touched; the worker never holds two session locks at once.
enum PublishOutcome {
    Pushed(Option<ReadyFrame>),
    Contended,
    Poisoned,
}

/// One completed desktop update waiting for a consumer.
pub(crate) struct ReadyFrame {
    pub(crate) slot: Arc<SurfaceSlot>,
    pub(crate) timestamp: frame_clock::MediaTimestamp,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// The session-owned ready queue: `BoundedQueue`, capacity two, `DropOldest`.
pub(crate) struct ReadyQueue {
    queue: BoundedQueue<ReadyFrame>,
}

impl Default for ReadyQueue {
    fn default() -> Self {
        Self {
            queue: BoundedQueue::new(
                NonZeroUsize::new(crate::surface::READY_QUEUE_CAPACITY)
                    .expect("the ready queue capacity is a nonzero constant"),
                OverflowPolicy::DropOldest,
            ),
        }
    }
}

impl ReadyQueue {
    pub(crate) fn push(&mut self, frame: ReadyFrame) -> Option<ReadyFrame> {
        self.queue.push(frame)
    }

    pub(crate) fn pop(&mut self) -> Option<ReadyFrame> {
        self.queue.pop()
    }

    pub(crate) fn stats(&self) -> QueueStats {
        self.queue.stats()
    }

    /// Removes every queued frame for teardown accounting.
    pub(crate) fn drain(&mut self) -> Vec<ReadyFrame> {
        let mut drained = Vec::with_capacity(self.queue.stats().depth);
        while let Some(frame) = self.queue.pop() {
            drained.push(frame);
        }
        drained
    }
}

/// A source discontinuity: the desktop duplication is no longer usable.
pub(crate) fn source_lost_error(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::SourceLost,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: "Stop the session, enumerate displays again and start a new capture session; Frame does not restart a lost desktop duplication automatically."
            .to_string(),
    }
}

/// A permission or secure-desktop denial.
pub(crate) fn permission_denied_error(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::PermissionDenied,
        message,
        recovery: RecoveryStatus::NoRecording,
        action:
            "Run Frame from an interactive desktop session that has access to the selected display."
                .to_string(),
    }
}

/// A capture feature this proof does not implement.
pub(crate) fn not_implemented_error(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::NotImplemented,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: "Select one landscape display with automatic source-matched FPS; other capture targets, rotations and fixed rates are later product work."
            .to_string(),
    }
}

/// An invalid numeric request.
pub(crate) fn invalid_request_error(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::InvalidRequest,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: "Correct the requested value and start a new capture session.".to_string(),
    }
}

/// A resource, watchdog or unexpected native failure.
pub(crate) fn internal_error(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message,
        recovery: RecoveryStatus::NoRecording,
        action:
            "Retry the capture session; if it persists, report the Windows build, GPU and driver."
                .to_string(),
    }
}

/// Describes a release failure, keeping the primary failure when there was one and
/// never replacing it.
pub(crate) fn merge_release_failure(
    primary: Option<RecorderError>,
    release: RecorderError,
) -> RecorderError {
    let mut error = primary.unwrap_or_else(|| RecorderError {
        code: release.code,
        message: String::new(),
        recovery: release.recovery,
        action: release.action.clone(),
    });
    error.message = if error.message.is_empty() {
        format!(
            "Releasing the acquired desktop frame failed: {}",
            release.message
        )
    } else {
        format!(
            "{} In addition, releasing the acquired desktop frame failed: {}",
            error.message, release.message
        )
    };
    error
}

#[cfg(test)]
mod preview_encoder_tests {
    use super::*;

    #[test]
    fn cpu_queue_replaces_pending_pixels_and_discards_them_on_generation_change() {
        let hub = PreviewHub::new(PreviewConfig::default(), 7);
        let encoder = PreviewEncoder {
            queue: Arc::new((Mutex::new(PreviewEncodeQueue::default()), Condvar::new())),
            epoch: Arc::new(AtomicU64::new(1)),
            thread: None,
        };
        for marker in [1, 2, 3] {
            encoder.submit(PreviewEncodeJob {
                pixels: vec![marker; 4],
                width: 1,
                height: 1,
                media_ns: Some(u64::from(marker)),
                readback_nanos: 0,
                hub: Arc::clone(&hub),
                generation: 7,
                subscription_epoch: 0,
                epoch: 0,
            });
        }
        let latest = encoder
            .take_pending_buffer()
            .expect("queue lock is free")
            .expect("one pending image remains");
        assert_eq!(latest, vec![3; 4]);
        assert!(encoder.take_pending_buffer().unwrap().is_none());
        encoder.submit(PreviewEncodeJob {
            pixels: vec![4; 4],
            width: 1,
            height: 1,
            media_ns: Some(4),
            readback_nanos: 0,
            hub,
            generation: 7,
            subscription_epoch: 0,
            epoch: 0,
        });
        encoder.invalidate();
        assert!(encoder.take_pending_buffer().unwrap().is_none());
    }
}
