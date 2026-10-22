//! Windows display capture (FRAME-0103, ADR 0005; corrected by FRAME-0103-R1).
//!
//! The backend implements `frame_capture::CaptureBackend` for one identity-rotation
//! display with the BGRA8 desktop image and `FpsMode::AutoMatchSource`. One worker
//! thread owns the native session; `WindowsCaptureSession` exposes the bounded ready
//! queue, one sticky terminal error and a human-readable diagnostics report. Nothing
//! in this module records, encodes, muxes or renders, and no UI or IPC contract is
//! touched.
//!
//! FRAME-0103-R1 corrections in this file:
//!
//! - `try_frame` takes **no blocking lock**: the terminal error is published once into
//!   a `OnceLock`, the payload reference is owned by the session and cleared on stop,
//!   and the ready queue is entered with `try_lock` only.
//! - A dead worker is never reported as an empty queue. A worker panic becomes a sticky
//!   `Internal` error, and a finished worker without cancellation is explicit.
//! - `stop` drops the session's own strong payload reference after the join, so a
//!   stopped session with no outstanding lease retains no GPU payload while its
//!   diagnostics remain available.
//! - The probe fails (and the example exits non-zero) when timestamps are not strictly
//!   increasing or the post-stop counters do not reconcile, and it uses the nominal
//!   source rate from its own start observation.

pub(crate) mod diagnostics;
#[allow(unsafe_code)]
pub(crate) mod native;
pub(crate) mod timing;
pub(crate) mod worker;

/// Typed read-only performance counters of one capture session (FRAME-0203).
pub use diagnostics::{CaptureCounters, CaptureSessionStats};

#[cfg(test)]
mod tests;

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError};
use std::time::{Duration, Instant};

use frame_capture::{
    CaptureBackend, CaptureCancellation, CaptureContext, CaptureSession, CapturedFrame,
};
use frame_clock::MasterClock;
use frame_contracts::{CaptureTarget, DisplayId, FpsMode, RecorderError};
use frame_metrics::QueueStats;

use crate::preview::{
    PreviewConfig, PreviewCounters, PreviewHub, PreviewSlotState, PreviewSubscriber,
    preview_payload_plan,
};
use crate::surface::{
    OwnedPayloadPlan, PayloadLifetime, SharedPayload, SurfacePool, WindowsSurface,
    plan_owned_payload, plan_owned_payload_with_bytes,
};
use diagnostics::{
    ClockEvidence, Counters, NativeDescription, ObservationLog, ReadbackEvidence, ReportInput,
    Snapshot, render,
};
use worker::{
    CaptureSource, ReadyFrame, ReadyQueue, WorkerContext, internal_error, invalid_request_error,
    not_implemented_error,
};

/// Preparation readiness watchdog (ADR 0005: five seconds).
const PREPARATION_WATCHDOG: Duration = Duration::from_secs(5);
/// How often the control thread re-checks cancellation while waiting for readiness.
const READINESS_POLL: Duration = Duration::from_millis(25);
/// Preview hub identities, so a consumer can tell two sources apart even when a session
/// number is reused after a restart (FRAME-0208).
static PREVIEW_GENERATION: AtomicU64 = AtomicU64::new(1);
/// Longest accepted probe duration (ADR 0005: 1..=3600 seconds).
const MAX_PROBE_SECONDS: u32 = 3600;
/// Window in which the probe expects capture to resume after releasing its timed
/// pressure leases.
const PROBE_RESUME_WINDOW: Duration = Duration::from_secs(2);
/// Window in which the probe collects leases to retain across stop.
const PROBE_RETAIN_WINDOW: Duration = Duration::from_secs(3);

/// What the native factory is allowed to know before it opens a session.
///
/// It carries no COM type, no clock and no queue: cancellation for the preparation,
/// whether opt-in readback was requested, and the payload-lifetime counters the
/// created owner must register with.
pub(crate) struct CapturePreparation<'a> {
    pub(crate) defer_frames: bool,
    pub(crate) cancellation: &'a CaptureCancellation,
    pub(crate) verify_pixels: bool,
    pub(crate) show_cursor: bool,
    pub(crate) lifetime: &'a Arc<PayloadLifetime>,
    /// Shared counters, so bounded native polling is observable in diagnostics.
    pub(crate) counters: &'a Arc<Counters>,
    /// This session's pool identity. The native backing records it so the hardware
    /// encoder can reject a lease that belongs to another capture pool (FRAME-0104)
    /// instead of registering a foreign texture.
    pub(crate) pool_id: u64,
    /// The worker's slot states, for WGC cross-process lease hand-back only.
    pub(crate) pool: &'a Arc<SurfacePool>,
}

/// Private capture options. The public backend always uses production behaviour;
/// only the diagnostic probe may enable bounded readback verification.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CaptureOptions {
    pub(crate) verify_pixels: bool,
    pub(crate) hdr_float: bool,
    pub(crate) show_cursor: bool,
    pub(crate) defer_frames: bool,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            verify_pixels: false,
            hdr_float: false,
            show_cursor: true,
            defer_frames: false,
        }
    }
}

/// A hub supplied at startup is attached before the worker can acquire its first
/// surface. A static desktop need not produce another surface after readiness.
pub(crate) struct PreviewSetup {
    pub(crate) config: PreviewConfig,
    pub(crate) hub: Option<Arc<PreviewHub>>,
}

/// Per-session state shared by the worker, the session facade and its leases.
pub(crate) struct SharedState {
    pub(crate) acquisition_ready: AtomicBool,
    pub(crate) wake: OnceLock<frame_capture::PipelineWake>,
    pub(crate) replay_sampling: AtomicBool,
    session_id: u64,
    width: u32,
    height: u32,
    verify_pixels: bool,
    pub(crate) pool: Arc<SurfacePool>,
    pub(crate) ready: Mutex<ReadyQueue>,
    /// The sticky terminal error. `OnceLock` publishes exactly one error and is read
    /// without any lock, so a consumer never waits on the worker to observe failure.
    terminal: OnceLock<RecorderError>,
    pub(crate) clock_evidence: Mutex<Option<ClockEvidence>>,
    pub(crate) readback: Mutex<Option<ReadbackEvidence>>,
    pub(crate) log: Mutex<ObservationLog>,
    pub(crate) native: Mutex<Option<NativeDescription>>,
    pub(crate) counters: Arc<Counters>,
    /// The preflighted payload plan this session was started with.
    payload_plan: OwnedPayloadPlan,
    /// Destruction evidence for the Frame-owned payload owners.
    pub(crate) lifetime: Arc<PayloadLifetime>,
    /// The nominal source rate from this session's own start observation.
    nominal_source_hz: OnceLock<Option<f64>>,
    pub(crate) worker_finished: AtomicBool,
    /// The preview request this session was started with; `None` when preview is off.
    pub(crate) preview_config: Mutex<Option<PreviewConfig>>,
    /// The attached preview hub. `None` means no preview work may run at all.
    pub(crate) preview: Mutex<Option<Arc<PreviewHub>>>,
    /// The preview counters of the current hub, so diagnostics report them.
    pub(crate) preview_counters: Mutex<Option<Arc<PreviewCounters>>>,
    /// The cost of the last generated preview image.
    pub(crate) preview_cost: Mutex<Option<PreviewSlotState>>,
}

impl SharedState {
    fn new(
        width: u32,
        height: u32,
        verify_pixels: bool,
        lifetime: Arc<PayloadLifetime>,
        payload_plan: OwnedPayloadPlan,
    ) -> Self {
        Self {
            acquisition_ready: AtomicBool::new(true),
            session_id: SESSION_SEQUENCE.fetch_add(1, Ordering::Relaxed),
            width,
            height,
            verify_pixels,
            pool: SurfacePool::new(payload_plan.slots_bytes),
            ready: Mutex::new(ReadyQueue::default()),
            terminal: OnceLock::new(),
            clock_evidence: Mutex::new(None),
            readback: Mutex::new(None),
            log: Mutex::new(ObservationLog::default()),
            native: Mutex::new(None),
            counters: Arc::new(Counters::default()),
            payload_plan,
            lifetime,
            nominal_source_hz: OnceLock::new(),
            wake: OnceLock::new(),
            replay_sampling: AtomicBool::new(false),
            worker_finished: AtomicBool::new(false),
            preview_config: Mutex::new(None),
            preview: Mutex::new(None),
            preview_counters: Mutex::new(Some(Arc::new(PreviewCounters::default()))),
            preview_cost: Mutex::new(None),
        }
    }

    /// Attaches one preview hub and returns its worker-side counters.
    ///
    /// This is a short try_lock only: a contended or poisoned preview slot fails the
    /// attach explicitly instead of blocking, and the capture session is otherwise
    /// untouched. It never opens a second capture and never waits for the worker.
    fn attach_preview(
        &self,
        hub: Arc<PreviewHub>,
        config: PreviewConfig,
    ) -> Result<Arc<PreviewCounters>, RecorderError> {
        let counters = Arc::clone(hub.counters_handle());
        match self.preview.try_lock() {
            Ok(mut slot) => {
                slot.replace(Arc::clone(&hub));
            }
            Err(TryLockError::WouldBlock) => {
                return Err(internal_error(
                    "The capture session's preview slot was busy, so the preview was not attached; retry once the session is idle."
                        .to_string(),
                ));
            }
            Err(TryLockError::Poisoned(_)) => return Err(poisoned_error()),
        }
        match self.preview_counters.try_lock() {
            Ok(mut slot) => {
                slot.replace(Arc::clone(&counters));
            }
            Err(TryLockError::WouldBlock) => {
                return Err(internal_error(
                    "The capture session's preview counters were busy, so the preview was not attached."
                        .to_string(),
                ));
            }
            Err(TryLockError::Poisoned(_)) => return Err(poisoned_error()),
        }
        match self.preview_config.try_lock() {
            Ok(mut slot) => {
                slot.replace(config);
            }
            Err(TryLockError::WouldBlock) => {
                return Err(internal_error(
                    "The capture session's preview configuration was busy, so the preview was not attached."
                        .to_string(),
                ));
            }
            Err(TryLockError::Poisoned(_)) => return Err(poisoned_error()),
        }
        Ok(counters)
    }

    /// Detaches the current hub and wakes every consumer waiting on it.
    fn detach_preview(&self) {
        let previous = self
            .preview
            .lock()
            .map(|mut slot| slot.take())
            .unwrap_or_else(|poison| poison.into_inner().take());
        if let Some(previous) = previous {
            previous.close();
        }
    }

    /// The one preview counter handle this session's worker reports into.
    ///
    /// The worker takes it once at start and the attached hub shares it, so preview
    /// work is counted exactly once no matter which component observes it.
    pub(crate) fn preview_counters_handle(&self) -> Arc<PreviewCounters> {
        match self.preview_counters.lock() {
            Ok(guard) => match guard.as_ref() {
                Some(counters) => Arc::clone(counters),
                None => Arc::new(PreviewCounters::default()),
            },
            Err(poison) => {
                let guard = poison.into_inner();
                match guard.as_ref() {
                    Some(counters) => Arc::clone(counters),
                    None => Arc::new(PreviewCounters::default()),
                }
            }
        }
    }

    pub(crate) fn width(&self) -> u32 {
        self.width
    }

    pub(crate) fn height(&self) -> u32 {
        self.height
    }

    /// Increments a counter.
    pub(crate) fn bump(counter: &AtomicU64) -> u64 {
        Counters::bump(counter)
    }

    /// Publishes the one sticky terminal error; later attempts keep the first.
    pub(crate) fn fail(&self, error: RecorderError) -> bool {
        let first = self.terminal.set(error).is_ok();
        self.notify_owner();
        first
    }

    pub(crate) fn notify_owner(&self) {
        if let Some(wake) = self.wake.get() {
            wake.notify();
        }
    }

    pub(crate) fn mark_worker_finished(&self) {
        self.worker_finished.store(true, Ordering::Release);
        self.notify_owner();
    }

    fn terminal_error(&self) -> Option<RecorderError> {
        self.terminal.get().cloned()
    }

    /// Locks a session mutex, treating poison as an explicit error rather than
    /// silently recovering the guard.
    pub(crate) fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>, RecorderError> {
        mutex.lock().map_err(|_| poisoned_error())
    }

    fn snapshot<T: Clone>(mutex: &Mutex<T>) -> Snapshot<T> {
        match mutex.try_lock() {
            Ok(guard) => Snapshot::Value(guard.clone()),
            Err(TryLockError::WouldBlock) => Snapshot::Contended,
            Err(TryLockError::Poisoned(_)) => Snapshot::Poisoned,
        }
    }

    /// Queue statistics without cloning the queue: the queue is deliberately not
    /// cloneable, and a contended queue is reported rather than waited for.
    fn queue_snapshot(mutex: &Mutex<ReadyQueue>) -> Snapshot<QueueStats> {
        match mutex.try_lock() {
            Ok(guard) => Snapshot::Value(guard.stats()),
            Err(TryLockError::WouldBlock) => Snapshot::Contended,
            Err(TryLockError::Poisoned(_)) => Snapshot::Poisoned,
        }
    }
}

/// Explicit poison error: a poisoned session lock is a defect, not something to
/// recover from silently.
pub(crate) fn poisoned_error() -> RecorderError {
    internal_error(
        "Capture session state was poisoned by a panic, so its contents cannot be trusted."
            .to_string(),
    )
}

/// Session identifiers, used only to prove that sessions and leases stay isolated.
static SESSION_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// One running Windows display capture session.
///
/// It owns the ready queue and the worker thread; its `Surface` is a
/// [`WindowsSurface`] lease that stays valid after the session stopped.
pub struct WindowsCaptureSession {
    shared: Arc<SharedState>,
    worker: Option<std::thread::JoinHandle<()>>,
    cancellation: CaptureCancellation,
    /// The session's own strong reference to the Frame-owned payload. Taken once at
    /// readiness, read without a lock, and dropped at stop so that "no lease" really
    /// means "no retained GPU payload".
    payload: Option<SharedPayload>,
    stopped: bool,
    stop_result: Option<RecorderError>,
}

/// Scoped scheduling for a Windows media owner. It exposes no native handles,
/// creates no thread and cannot be moved to a different thread after joining.
pub use native::CaptureScheduling as WindowsMediaScheduling;

impl std::fmt::Debug for WindowsCaptureSession {
    /// Deliberately minimal: a session is identified by its session number and
    /// whether it is stopped, not by its native resources.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WindowsCaptureSession")
            .field("session_id", &self.shared.session_id)
            .field("stopped", &self.stopped)
            .finish_non_exhaustive()
    }
}

impl WindowsCaptureSession {
    /// Allocate the capture device and fixed surface pool, but acquire no frames
    /// until encoder/container preparation has completed. Preview shares this
    /// same worker and is installed before acquisition starts.
    pub fn prepare_recording_display(
        display_id: &DisplayId,
        context: CaptureContext,
        show_cursor: bool,
        hdr_float: bool,
        hub: Option<Arc<PreviewHub>>,
    ) -> Result<Self, RecorderError> {
        let preview = hub.map(|hub| PreviewSetup {
            config: hub.config(),
            hub: Some(hub),
        });
        start_session(
            &CaptureTarget::Display(display_id.clone()),
            FpsMode::AutoMatchSource,
            context,
            CaptureOptions {
                show_cursor,
                hdr_float,
                defer_frames: true,
                ..CaptureOptions::default()
            },
            preview,
        )
    }
    /// Starts display preview with its consumer's hub installed before acquisition.
    /// The caller subscribes before calling this method and owns that subscription.
    pub fn start_with_preview_hub(
        target: &CaptureTarget,
        context: CaptureContext,
        hub: Arc<PreviewHub>,
    ) -> Result<Self, RecorderError> {
        Self::start_with_preview_hub_cursor(target, context, hub, true)
    }

    pub fn start_with_preview_hub_cursor(
        target: &CaptureTarget,
        context: CaptureContext,
        hub: Arc<PreviewHub>,
        show_cursor: bool,
    ) -> Result<Self, RecorderError> {
        let config = hub.config();
        config.validate()?;
        start_session(
            target,
            FpsMode::AutoMatchSource,
            context,
            CaptureOptions {
                show_cursor,
                ..CaptureOptions::default()
            },
            Some(PreviewSetup {
                config,
                hub: Some(hub),
            }),
        )
    }

    /// Explicit FP16 display capture. No preview, region crop or camera composition is
    /// attached: those consumers currently accept BGRA8 surfaces only.
    pub fn start_hdr_display(
        display_id: &DisplayId,
        context: CaptureContext,
    ) -> Result<Self, RecorderError> {
        start_session(
            &CaptureTarget::Display(display_id.clone()),
            FpsMode::AutoMatchSource,
            context,
            CaptureOptions {
                verify_pixels: false,
                hdr_float: true,
                show_cursor: false,
                defer_frames: false,
            },
            None,
        )
    }
    /// Bounded human-readable capture diagnostics.
    ///
    /// Every section is read with `try_lock`: a contended or poisoned section is
    /// reported as such instead of stalling the caller, the worker or a consumer.
    /// Live counters are a snapshot, not an atomic global view, and the report says so.
    pub fn diagnostics(&self) -> Result<String, RecorderError> {
        let counters = self.shared.counters.snapshot();
        let mut unavailable = Vec::new();
        let queue = match SharedState::queue_snapshot(&self.shared.ready) {
            Snapshot::Value(queue) => Some(queue),
            Snapshot::Contended => {
                unavailable.push("queue(contended)");
                None
            }
            Snapshot::Poisoned => {
                unavailable.push("queue(poisoned)");
                None
            }
        };
        let clock = match SharedState::snapshot(&self.shared.clock_evidence) {
            Snapshot::Value(clock) => clock,
            Snapshot::Contended => {
                unavailable.push("clock(contended)");
                None
            }
            Snapshot::Poisoned => {
                unavailable.push("clock(poisoned)");
                None
            }
        };
        let readback = match SharedState::snapshot(&self.shared.readback) {
            Snapshot::Value(readback) => readback,
            Snapshot::Contended => {
                unavailable.push("readback(contended)");
                None
            }
            Snapshot::Poisoned => {
                unavailable.push("readback(poisoned)");
                None
            }
        };
        let native = match SharedState::snapshot(&self.shared.native) {
            Snapshot::Value(native) => native,
            Snapshot::Contended => {
                unavailable.push("native(contended)");
                None
            }
            Snapshot::Poisoned => {
                unavailable.push("native(poisoned)");
                None
            }
        };
        let log = match SharedState::snapshot(&self.shared.log) {
            Snapshot::Value(log) => log,
            Snapshot::Contended => {
                unavailable.push("samples(contended)");
                ObservationLog::default()
            }
            Snapshot::Poisoned => {
                unavailable.push("samples(poisoned)");
                ObservationLog::default()
            }
        };
        let preview = match SharedState::snapshot(&self.shared.preview_cost) {
            Snapshot::Value(preview) => preview,
            Snapshot::Contended => {
                unavailable.push("preview(contended)");
                None
            }
            Snapshot::Poisoned => {
                unavailable.push("preview(poisoned)");
                None
            }
        };
        let preview_counters = match SharedState::snapshot(&self.shared.preview_counters) {
            Snapshot::Value(counters) => counters,
            Snapshot::Contended => {
                unavailable.push("preview_counters(contended)");
                None
            }
            Snapshot::Poisoned => {
                unavailable.push("preview_counters(poisoned)");
                None
            }
        };
        let slots = self.shared.pool.counts();
        let terminal = self.shared.terminal_error();
        let payload_bytes = self.shared.pool.payload_bytes();
        let payload_plan = self.shared.payload_plan;
        let preview_config = match SharedState::snapshot(&self.shared.preview_config) {
            Snapshot::Value(config) => config,
            Snapshot::Contended => {
                unavailable.push("preview_config(contended)");
                None
            }
            Snapshot::Poisoned => {
                unavailable.push("preview_config(poisoned)");
                None
            }
        };
        let preview_hub = match SharedState::snapshot(&self.shared.preview) {
            Snapshot::Value(hub) => hub.as_ref().map(|hub| {
                (
                    hub.subscriber_count(),
                    hub.enabled(),
                    hub.generation(),
                    hub.counters(),
                )
            }),
            Snapshot::Contended => {
                unavailable.push("preview_hub(contended)");
                None
            }
            Snapshot::Poisoned => {
                unavailable.push("preview_hub(poisoned)");
                None
            }
        };
        let preview_attached = preview_hub.is_some();
        let preview_generation = preview_hub.as_ref().map(|hub| hub.2);
        let preview_subscribers = preview_hub.as_ref().map(|hub| hub.0);
        let preview_enabled = preview_hub.as_ref().map(|hub| hub.1);
        let preview_hub_counters = preview_hub.as_ref().map(|hub| hub.3).or_else(|| {
            preview_counters
                .as_ref()
                .map(|counters| counters.snapshot())
        });
        Ok(render(&ReportInput {
            session_id: self.shared.session_id,
            stopped: self.stopped,
            worker_finished: self.shared.worker_finished.load(Ordering::Acquire),
            native: native.as_ref(),
            counters,
            queue: queue.unwrap_or_default(),
            queue_available: queue.is_some(),
            slots,
            payload_bytes,
            clock,
            log: &log,
            terminal: terminal.as_ref(),
            verify_pixels: self.shared.verify_pixels,
            consumer_held: slots.consumer_owned,
            readback,
            unavailable,
            lifetime_live: self.shared.lifetime.live(),
            lifetime_created: self.shared.lifetime.created(),
            session_payload_held: self.payload.is_some(),
            payload_plan_bytes: payload_plan.total_bytes,
            nominal_source_hz: self.shared.nominal_source_hz.get().copied().flatten(),
            preview_attached,
            preview_config,
            preview_generation,
            preview_subscribers,
            preview_enabled,
            preview_counters: preview_hub_counters,
            preview: preview.as_ref().copied(),
        }))
    }

    /// The nominal source rate observed by this session's start, if any.
    fn nominal_source_hz(&self) -> Option<f64> {
        self.shared.nominal_source_hz.get().copied().flatten()
    }

    /// Typed read-only counters for a running or stopped capture session.
    pub fn stats(&self) -> CaptureSessionStats {
        CaptureSessionStats {
            session_id: self.shared.session_id,
            stopped: self.stopped,
            worker_finished: self.shared.worker_finished.load(Ordering::Acquire),
            nominal_source_hz: self.nominal_source_hz(),
            ready_queue: match SharedState::queue_snapshot(&self.shared.ready) {
                Snapshot::Value(queue) => Some(queue),
                Snapshot::Contended | Snapshot::Poisoned => None,
            },
            counters: CaptureCounters::from_snapshot(&self.shared.counters.snapshot()),
        }
    }

    /// Attaches one preview hub to this *live* capture session (FRAME-0208).
    ///
    /// The preview is produced from the same session's own surfaces on the session's own
    /// worker thread: this call opens no second display capture, no device and no
    /// duplication. It is non-blocking and does not wait for the worker, and the session
    /// keeps generating images only while at least one subscriber is attached, so a
    /// detached preview stops the work instead of idling it.
    ///
    /// It fails explicitly when the session was started without a preview request, when
    /// it has already been stopped, or when its preview slot is busy; it never silently
    /// accepts a preview that will not run.
    pub fn attach_preview(
        &self,
        hub: &Arc<PreviewHub>,
        config: PreviewConfig,
    ) -> Result<(), RecorderError> {
        if self.stopped {
            return Err(internal_error(
                "This capture session is stopped, so a preview cannot be attached to it."
                    .to_string(),
            ));
        }
        if self.shared.terminal_error().is_some() {
            return Err(internal_error(
                "This capture session already reported a terminal error, so a preview cannot be attached to it."
                    .to_string(),
            ));
        }
        let Some(session_config) = *SharedState::lock(&self.shared.preview_config)? else {
            return Err(internal_error(
                "This capture session was started without a preview request, so no preview work may run on it."
                    .to_string(),
            ));
        };
        if session_config != config {
            return Err(invalid_request_error(format!(
                "The capture session was started for a {session_config:?} preview, so a {config:?} preview cannot be attached to it."
            )));
        }
        self.shared.attach_preview(Arc::clone(hub), config)?;
        Ok(())
    }

    /// The preview hub currently attached to this session, if any.
    pub fn preview_hub(&self) -> Option<Arc<PreviewHub>> {
        SharedState::lock(&self.shared.preview)
            .ok()
            .and_then(|slot| slot.clone())
    }

    /// Attaches a subscriber to this session's preview hub.
    ///
    /// The returned subscriber must be kept alive while the preview is wanted: dropping
    /// it detaches the last consumer, and the capture worker then stops producing
    /// images without any join or wait.
    pub fn start_preview(&self) -> Result<PreviewSubscriber, RecorderError> {
        let hub = self.preview_hub().ok_or_else(|| {
            internal_error(
                "This capture session has no preview hub attached, so a subscriber cannot be created."
                    .to_string(),
            )
        })?;
        Ok(hub.subscribe())
    }
}

impl CaptureSession for WindowsCaptureSession {
    type Surface = WindowsSurface;

    fn preparation_surface(&mut self) -> Result<Option<WindowsSurface>, RecorderError> {
        if self.stopped || self.shared.acquisition_ready.load(Ordering::Acquire) {
            return Ok(None);
        }
        if let Some(error) = self.shared.terminal_error() {
            return Err(error);
        }
        let payload = self
            .payload
            .clone()
            .ok_or_else(|| internal_error("Prepared capture has no surface backing.".into()))?;
        let slot = self.shared.pool.try_acquire_copy_slot().ok_or_else(|| {
            internal_error(
                "Prepared capture has no free surface; acquisition has not started.".into(),
            )
        })?;
        slot.mark_consumer_owned();
        Ok(Some(WindowsSurface::preparation(slot, payload)))
    }

    fn begin_frames(&mut self) -> Result<(), RecorderError> {
        if self.shared.acquisition_ready.load(Ordering::Acquire) {
            return Ok(());
        }
        if let Some(error) = self.shared.terminal_error() {
            return Err(error);
        }
        if self.stopped || self.cancellation.is_cancelled() {
            return Err(timing::preparation_cancelled());
        }
        // Preparation views must be released before their textures can be copied.
        if !self.shared.acquisition_ready.load(Ordering::Acquire)
            && self.shared.pool.counts().occupied() != 0
        {
            return Err(internal_error(
                "Release preparation surfaces before arming capture.".into(),
            ));
        }
        self.shared.acquisition_ready.store(true, Ordering::Release);
        crate::cursor_preview_diagnostics::record(
            "capture_armed",
            None,
            || serde_json::json!({"session_id": self.shared.session_id}),
        );
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
        Ok(())
    }

    fn set_wake_signal(&mut self, wake: frame_capture::PipelineWake) {
        let _ = self.shared.wake.set(wake);
        self.shared.notify_owner();
    }
    fn set_replay_sampling(&mut self, enabled: bool) {
        self.shared
            .replay_sampling
            .store(enabled, Ordering::Release);
    }

    fn pipeline_stats(&self) -> Option<frame_capture::CapturePipelineStats> {
        let stats = self.stats();
        let c = stats.counters;
        Some(frame_capture::CapturePipelineStats {
            source_presentations: c.native_desktop_presentations,
            captured_frames: c.copy_completions,
            captured_unique_frames: c.captured_desktop_presentations,
            pool_pressure_drops: c.pool_exhausted,
            ready_queue_drops: c.ready_evictions,
            contention_drops: c.worker_contention_drops,
            upstream_accumulated_updates: c.upstream_accumulated_updates,
            ready_depth: stats.ready_queue.map(|q| q.depth as u32),
            ready_capacity: crate::surface::READY_QUEUE_CAPACITY as u32,
        })
    }

    fn try_frame(&mut self) -> Result<Option<CapturedFrame<Self::Surface>>, RecorderError> {
        // Nonblocking by construction: the sticky error is read without a lock and
        // takes precedence over queued frames.
        if let Some(error) = self.shared.terminal_error() {
            return Err(error);
        }
        if self.stopped {
            return Ok(None);
        }
        if self.shared.worker_finished.load(Ordering::Acquire) && !self.cancellation.is_cancelled()
        {
            // A worker that stopped without cancellation and without recording an error
            // is a defect: report it instead of returning a perpetual empty queue.
            return Err(internal_error(
                "The capture worker stopped without reporting an error, so no further update can be delivered."
                    .to_string(),
            ));
        }
        let Some(payload) = self.payload.clone() else {
            // Readiness published no payload: nothing can be leased. This is only
            // reachable after stop, which returns above.
            return Ok(None);
        };
        let popped = match self.shared.ready.try_lock() {
            Ok(mut queue) => queue.pop(),
            Err(TryLockError::WouldBlock) => {
                // Short try_lock only: contention is not an error and not shutdown.
                SharedState::bump(&self.shared.counters.consumer_contention);
                return Ok(None);
            }
            Err(TryLockError::Poisoned(_)) => {
                // Poison is persistent, not a transient error: publish it so later
                // calls, stop and diagnostics all observe the same failure.
                let error = poisoned_error();
                self.shared.fail(error.clone());
                return Err(error);
            }
        };
        let Some(frame) = popped else {
            return Ok(None);
        };
        let ReadyFrame {
            slot,
            timestamp,
            width,
            height,
        } = frame;
        slot.mark_consumer_owned();
        SharedState::bump(&self.shared.counters.dequeued);
        Ok(Some(CapturedFrame {
            surface: WindowsSurface::new(slot, payload),
            timestamp,
            width,
            height,
        }))
    }

    /// Idempotent teardown on a control thread.
    ///
    /// Teardown happens once; later calls return the same result. A capture failure
    /// observed by the worker is reported here as well as by `try_frame` and
    /// `diagnostics`, so it is never lost. Outstanding leases stay valid, and the
    /// session's own payload reference is released so that diagnostics remain
    /// available without retaining GPU memory.
    fn stop(&mut self) -> Result<(), RecorderError> {
        if !self.stopped {
            self.stopped = true;
            let started = Instant::now();
            // The preview is closed before the join, so a consumer blocked on the hub
            // wakes with "closed" instead of waiting for an image that will never come,
            // and the worker's next generation check sees no subscriber.
            self.shared.detach_preview();
            self.cancellation.cancel();
            if let Some(worker) = &self.worker {
                worker.thread().unpark();
            }
            let mut failure = None;
            if let Some(handle) = self.worker.take()
                && handle.join().is_err()
            {
                failure = Some(internal_error(
                    "The capture worker thread panicked, so native resources may not have been released cleanly."
                        .to_string(),
                ));
            }
            // Queued frames are drained after the join, so the accounting is stable and
            // neither the worker nor the session counts a frame twice. A poisoned queue
            // is salvaged *and* reported rather than silently recovered.
            match self.shared.ready.lock() {
                Ok(mut queue) => {
                    for frame in queue.drain() {
                        frame.slot.mark_free();
                        SharedState::bump(&self.shared.counters.shutdown_discarded_queued);
                    }
                }
                Err(poison) => {
                    let mut queue = poison.into_inner();
                    for frame in queue.drain() {
                        frame.slot.mark_free();
                        SharedState::bump(&self.shared.counters.shutdown_discarded_queued);
                    }
                    failure = Some(failure.unwrap_or_else(poisoned_error));
                }
            }
            // Release the session's strong payload reference: with no lease, the GPU
            // payload is destroyed here, while `diagnostics()` keeps working.
            self.payload = None;
            let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
            self.shared
                .counters
                .teardown_nanos
                .store(nanos, Ordering::Relaxed);
            self.stop_result = failure.or_else(|| self.shared.terminal_error());
        }
        match &self.stop_result {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }
}

impl Drop for WindowsCaptureSession {
    fn drop(&mut self) {
        // Safety net only: cancels and joins. It never runs on a UI thread by
        // contract, and it never detaches the worker.
        let _ = self.stop();
    }
}

/// One live preview attached to a running capture session (FRAME-0208).
///
/// It owns the session that produces the surfaces, the hub the images are published
/// through and the subscriber that keeps the worker generating. Because the preview is
/// produced from the session's own surfaces, the idle-preview case is the *same* owner
/// as the recording case: a caller that switches from preview to recording keeps this
/// session and this hub instead of opening a second display capture. A caller that
/// stops here first gets an explicit teardown — the session is stopped and joined, and
/// the hub is closed before the join — so a recording can then start without two
/// duplications and without handing a surface across owners.
pub struct ThumbnailPreviewSession {
    hub: Arc<PreviewHub>,
    generation: u64,
    session: WindowsCaptureSession,
    subscriber: Option<PreviewSubscriber>,
}

impl std::fmt::Debug for ThumbnailPreviewSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ThumbnailPreviewSession")
            .field("generation", &self.generation)
            .field("config", &self.hub.config())
            .field("subscribers", &self.hub.subscriber_count())
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

/// Starts one capture session with a live preview attached to it.
///
/// The preview request is validated first, and the one session is started with the
/// preview recorded, so the worker may produce images from the same surfaces it hands
/// to a recorder. The returned value owns the session; dropping it closes the preview
/// and joins the capture worker, so no worker and no hub is ever left detached.
pub fn start_thumbnail_preview_session(
    target: &CaptureTarget,
    fps: FpsMode,
    context: CaptureContext,
    config: PreviewConfig,
) -> Result<ThumbnailPreviewSession, RecorderError> {
    let mut session = start_preview_session(target, fps, context, config)?;
    let generation = PREVIEW_GENERATION.fetch_add(1, Ordering::Relaxed);
    let hub = PreviewHub::new(config, generation);
    if let Err(error) = session.attach_preview(&hub, config) {
        let _ = session.stop();
        return Err(error);
    }
    let subscriber = hub.subscribe();
    Ok(ThumbnailPreviewSession {
        hub,
        generation,
        session,
        subscriber: Some(subscriber),
    })
}

impl ThumbnailPreviewSession {
    /// The hub this preview publishes through.
    pub fn hub(&self) -> &Arc<PreviewHub> {
        &self.hub
    }

    /// The source generation of this preview, so a consumer can reject an image that
    /// belongs to an earlier source.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The capture session that produces the surfaces.
    pub fn session(&self) -> &WindowsCaptureSession {
        &self.session
    }

    /// The capture session, mutably, for a caller that owns both paths.
    ///
    /// A recording owner needs `try_frame` on the same session the preview reads, so the
    /// two paths share one duplication and one slot pool instead of opening a second
    /// display capture.
    pub fn session_mut(&mut self) -> &mut WindowsCaptureSession {
        &mut self.session
    }

    /// Attaches one more consumer and returns it.
    ///
    /// A caller that detached with [`ThumbnailPreviewSession::detach_subscriber`] can
    /// resume the preview with this; the worker notices the subscriber on its next
    /// completed surface and starts producing images again.
    pub fn attach_subscriber(&self) -> PreviewSubscriber {
        self.hub.subscribe()
    }

    /// Detaches the last subscriber, after which the worker produces no more images.
    ///
    /// This is the "preview closed" path: it returns immediately and never waits for
    /// the worker to observe the change.
    pub fn detach_subscriber(&mut self) {
        self.subscriber = None;
    }

    /// Reads the current session diagnostics, including the preview counters.
    pub fn diagnostics(&self) -> Result<String, RecorderError> {
        self.session.diagnostics()
    }

    /// Stops the session explicitly: the preview is closed first, then the capture
    /// worker is cancelled and joined.
    pub fn stop(&mut self) -> Result<(), RecorderError> {
        self.subscriber = None;
        self.hub.close();
        self.session.stop()
    }
}

impl Drop for ThumbnailPreviewSession {
    fn drop(&mut self) {
        // Close and detach before the join, so a consumer blocked on the hub wakes with
        // "closed" instead of waiting for a frame that will never come.
        self.subscriber = None;
        self.hub.close();
    }
}

impl CaptureBackend for crate::PlatformBackend {
    type Session = WindowsCaptureSession;

    fn start(
        &mut self,
        target: &CaptureTarget,
        fps: FpsMode,
        context: CaptureContext,
    ) -> Result<Self::Session, RecorderError> {
        start_session(target, fps, context, CaptureOptions::default(), None)
    }
}

pub fn start_display_with_cursor(
    target: &CaptureTarget,
    fps: FpsMode,
    context: CaptureContext,
    show_cursor: bool,
) -> Result<WindowsCaptureSession, RecorderError> {
    start_session(
        target,
        fps,
        context,
        CaptureOptions {
            show_cursor,
            ..CaptureOptions::default()
        },
        None,
    )
}

/// Starts one capture session that may carry a bounded live preview (FRAME-0208).
///
/// This is the same production session as [`CaptureBackend::start`]: same duplication,
/// same worker, same slot pool and same clock. The only difference is that the session
/// records a preview request, after which the caller may attach a [`PreviewHub`] to the
/// running session with [`WindowsCaptureSession::attach_preview`]. Because the preview is
/// produced from this session's own surfaces, no second display capture is opened.
pub fn start_preview_session(
    target: &CaptureTarget,
    fps: FpsMode,
    context: CaptureContext,
    config: PreviewConfig,
) -> Result<WindowsCaptureSession, RecorderError> {
    start_preview_session_with_cursor(target, fps, context, config, true)
}

pub fn start_preview_session_with_cursor(
    target: &CaptureTarget,
    fps: FpsMode,
    context: CaptureContext,
    config: PreviewConfig,
    show_cursor: bool,
) -> Result<WindowsCaptureSession, RecorderError> {
    config.validate()?;
    start_session(
        target,
        fps,
        context,
        CaptureOptions {
            show_cursor,
            ..CaptureOptions::default()
        },
        Some(PreviewSetup { config, hub: None }),
    )
}

/// Query whether the selected display is actively signalling PQ/BT.2020. `None`
/// means the OS exposes no IDXGIOutput6 answer; it must not be interpreted as SDR.
pub fn active_hdr_display(display_id: &DisplayId) -> Result<Option<bool>, RecorderError> {
    let selection = crate::display::capture_output(&display_id.0)?;
    native::active_hdr(&selection)
}

/// Validates the request, prepares the worker and waits for bounded readiness.
fn start_session(
    target: &CaptureTarget,
    fps: FpsMode,
    context: CaptureContext,
    options: CaptureOptions,
    preview: Option<PreviewSetup>,
) -> Result<WindowsCaptureSession, RecorderError> {
    let display_id = validate_request(target, fps)?;
    if context.cancellation.is_cancelled() {
        return Err(timing::preparation_cancelled());
    }

    // Re-resolve the stable identity to the output that is attached right now. This
    // single observation also supplies the nominal source rate reported by the probe.
    let selection = crate::display::capture_output(&display_id.0)?;
    // Preflight the whole owned payload (including the opt-in staging texture) before
    // any texture can be created.
    let plan = if options.hdr_float {
        plan_owned_payload_with_bytes(selection.width, selection.height, false, 8)?
    } else {
        plan_owned_payload(selection.width, selection.height, options.verify_pixels)?
    };
    if options.hdr_float && (preview.is_some() || options.verify_pixels) {
        return Err(RecorderError::unsupported(
            "FP16 HDR capture does not support BGRA8 preview or pixel verification.",
        ));
    }
    if let Some(setup) = preview.as_ref() {
        // A preview that cannot fit its own bounded payload is refused before the
        // session starts, not after the first surface arrives.
        preview_payload_plan(
            setup.config,
            selection.width,
            selection.height,
            plan.total_bytes,
            0,
        )?;
    }

    let factory_selection = selection.clone();
    start_with_factory(
        selection,
        plan,
        context,
        options,
        preview,
        move |preparation| native::open(&factory_selection, preparation, options.hdr_float),
    )
}

/// Worker start with an injectable native factory, so preparation cancellation,
/// preparation failure and worker death are testable without a GPU.
fn start_with_factory<S, F>(
    selection: crate::display::CaptureOutput,
    plan: OwnedPayloadPlan,
    context: CaptureContext,
    options: CaptureOptions,
    preview: Option<PreviewSetup>,
    factory: F,
) -> Result<WindowsCaptureSession, RecorderError>
where
    S: CaptureSource + 'static,
    F: FnOnce(&CapturePreparation<'_>) -> Result<S, RecorderError> + Send + 'static,
{
    start_source_with_factory(
        (selection.width, selection.height),
        selection.refresh_rate_hz,
        plan,
        context,
        options,
        preview,
        factory,
    )
}

/// Shared worker preparation for display and WGC. A source supplies its own native
/// factory while the queue, readiness, preview and joined teardown stay common.
pub(crate) fn start_source_with_factory<S, F>(
    geometry: (u32, u32),
    nominal_source_hz: Option<f64>,
    plan: OwnedPayloadPlan,
    context: CaptureContext,
    options: CaptureOptions,
    preview: Option<PreviewSetup>,
    factory: F,
) -> Result<WindowsCaptureSession, RecorderError>
where
    S: CaptureSource + 'static,
    F: FnOnce(&CapturePreparation<'_>) -> Result<S, RecorderError> + Send + 'static,
{
    let lifetime = PayloadLifetime::new();
    let shared = Arc::new(SharedState::new(
        geometry.0,
        geometry.1,
        options.verify_pixels,
        Arc::clone(&lifetime),
        plan,
    ));
    shared
        .acquisition_ready
        .store(!options.defer_frames, Ordering::Release);
    let preview_config = preview.as_ref().map(|setup| setup.config);
    if let Some(setup) = preview.as_ref()
        && let Some(hub) = setup.hub.as_ref()
    {
        shared.attach_preview(Arc::clone(hub), setup.config)?;
    }
    if let Some(config) = preview_config
        && let Ok(mut slot) = shared.preview_config.lock()
    {
        slot.replace(config);
    }
    let _ = shared.nominal_source_hz.set(nominal_source_hz).is_ok();
    let (ready_tx, ready_rx): (SyncSender<Result<SharedPayload, RecorderError>>, _) =
        sync_channel(1);

    let thread_shared = Arc::clone(&shared);
    let thread_lifetime = Arc::clone(&lifetime);
    let worker_shared = Arc::clone(&shared);
    let worker_cancellation = context.cancellation.clone();
    let worker_clock: Arc<MasterClock> = context.clock.clone();
    let worker_context = WorkerContext {
        shared: Arc::clone(&worker_shared),
        clock: worker_clock,
        cancellation: worker_cancellation.clone(),
        verify_pixels: options.verify_pixels,
        preview: preview_config,
    };
    let worker_counters = Arc::clone(&shared.counters);
    // Keep the channel connected until after a panic has been published, so the control
    // thread never observes a disconnect before the sticky error exists.
    let ready_keepalive = ready_tx.clone();
    let handle = std::thread::Builder::new()
        .name("frame-capture-worker".to_string())
        .spawn(move || {
            let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                let preparation = CapturePreparation {
                    defer_frames: options.defer_frames,
                    cancellation: &worker_cancellation,
                    verify_pixels: options.verify_pixels,
                    show_cursor: options.show_cursor,
                    lifetime: &thread_lifetime,
                    counters: &worker_counters,
                    pool_id: worker_context.shared.pool.id(),
                    pool: &worker_context.shared.pool,
                };
                match factory(&preparation) {
                    Ok(source) => worker::run(source, worker_context, ready_tx),
                    Err(error) => {
                        worker_context.shared.fail(error.clone());
                        worker_context.shared.mark_worker_finished();
                        let _ = ready_tx.send(Err(error));
                    }
                }
            }));
            if outcome.is_err() {
                // A panic is a defect, not an empty queue: publish it as the sticky
                // terminal error. RAII still unwound the worker's native resources.
                thread_shared.fail(internal_error(
                    "The capture worker thread panicked, so the session was terminated."
                        .to_string(),
                ));
            }
            thread_shared.mark_worker_finished();
            drop(ready_keepalive);
        })
        .map_err(|error| {
            internal_error(format!(
                "The capture worker thread could not be started: {error}"
            ))
        })?;

    let mut session = WindowsCaptureSession {
        shared: Arc::clone(&shared),
        worker: Some(handle),
        cancellation: context.cancellation.clone(),
        payload: None,
        stopped: false,
        stop_result: None,
    };

    let deadline = Instant::now() + PREPARATION_WATCHDOG;
    let payload = loop {
        if context.cancellation.is_cancelled() {
            let _ = session.stop();
            return Err(timing::preparation_cancelled());
        }
        match ready_rx.recv_timeout(READINESS_POLL) {
            Ok(Ok(payload)) => break payload,
            Ok(Err(error)) => {
                let _ = session.stop();
                return Err(error);
            }
            Err(RecvTimeoutError::Timeout) => {
                if Instant::now() >= deadline {
                    session.cancellation.cancel();
                    let _ = session.stop();
                    return Err(internal_error(format!(
                        "Capture preparation did not become ready within {} ms, past the preparation watchdog.",
                        PREPARATION_WATCHDOG.as_millis()
                    )));
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                // The worker reported nothing and exited: its error, if any, is stored.
                let recorded = session.shared.terminal_error();
                let _ = session.stop();
                return Err(recorded.unwrap_or_else(|| {
                    internal_error(
                        "The capture worker stopped before reporting readiness, without recording an error."
                            .to_string(),
                    )
                }));
            }
        }
    };
    session.payload = Some(payload);

    // The native source re-resolves the selection immediately before duplication
    // activation, then checks its geometry/rotation/format before acquiring. A
    // prepared recording activates on its first consumer poll. Separate OS reads
    // remain non-atomic; cancellation is rechecked on this control owner as well.
    if context.cancellation.is_cancelled() {
        let _ = session.stop();
        return Err(timing::preparation_cancelled());
    }

    Ok(session)
}

/// Validates the requested target and frame-rate mode without touching native state.
fn validate_request(target: &CaptureTarget, fps: FpsMode) -> Result<DisplayId, RecorderError> {
    let display_id = match target {
        CaptureTarget::Display(display_id) => display_id.clone(),
        CaptureTarget::Window(_) => {
            return Err(not_implemented_error(
                "Window capture is not implemented by this display capture proof.".to_string(),
            ));
        }
        CaptureTarget::Region(_) => {
            return Err(not_implemented_error(
                "Region capture is not implemented by this display capture proof.".to_string(),
            ));
        }
        CaptureTarget::Game(_) => {
            return Err(not_implemented_error(
                "Game capture is not implemented by this display capture proof.".to_string(),
            ));
        }
    };
    match fps {
        FpsMode::AutoMatchSource => {}
        FpsMode::Fixed(value) => {
            if !value.is_finite() || value <= 0.0 {
                return Err(invalid_request_error(format!(
                    "A fixed frame rate of {value} is not a finite positive number."
                )));
            }
            return Err(not_implemented_error(format!(
                "A fixed {value} FPS request is not implemented by this display capture proof; automatic source-matched FPS follows desktop updates instead."
            )));
        }
    }
    Ok(display_id)
}

/// What the probe observed during its timed pressure phase.
#[derive(Default)]
struct PressurePhase {
    held: Vec<WindowsSurface>,
    started: Option<Instant>,
    /// Set when the timed phase released its leases; retention beyond this instant is
    /// never claimed by the report.
    released: bool,
    held_ms: u64,
    exhausted_during_pressure: u64,
    resumed_after: Option<u64>,
    resume_deadline: Option<Instant>,
    resumed_within_window: Option<bool>,
}

/// Runs the diagnostic capture probe for one display and returns its report.
///
/// Windows-only diagnostic helper: it exercises the production session (the same
/// backend, worker, queue, lease and clock path), writes no file and creates no
/// recording. Invalid options fail before any native work. The probe returns an error
/// when its own acceptance checks fail, so the example cannot print a failure
/// alongside a success exit code.
pub fn capture_probe(
    display_id: &DisplayId,
    seconds: u32,
    stall_consumer_ms: u32,
    verify_pixels: bool,
) -> Result<String, RecorderError> {
    if !(1..=MAX_PROBE_SECONDS).contains(&seconds) {
        return Err(invalid_request_error(format!(
            "The probe duration must be between 1 and {MAX_PROBE_SECONDS} seconds, not {seconds}."
        )));
    }
    let duration = Duration::from_secs(u64::from(seconds));
    let stall = Duration::from_millis(u64::from(stall_consumer_ms));
    if stall > duration {
        return Err(invalid_request_error(format!(
            "The consumer stall of {stall_consumer_ms} ms cannot exceed the {seconds} second probe duration."
        )));
    }

    let clock = Arc::new(MasterClock::default());
    let cancellation = CaptureCancellation::default();
    let context = CaptureContext {
        clock: Arc::clone(&clock),
        cancellation: cancellation.clone(),
    };
    let target = CaptureTarget::Display(display_id.clone());
    let options = CaptureOptions {
        verify_pixels,
        hdr_float: false,
        show_cursor: true,
        defer_frames: false,
    };
    let mut session = start_session(&target, FpsMode::AutoMatchSource, context, options, None)?;
    // The nominal rate comes from the same start observation that resolved the output,
    // not from a second enumeration that could fail or disagree.
    let source_hz = session.nominal_source_hz();

    let started = Instant::now();
    let mut consumed = 0u64;
    let mut monotonic = true;
    let mut decreasing: Option<(u64, u64)> = None;
    let mut previous: Option<u64> = None;
    let mut first_media = None;
    let mut last_media = None;
    let mut pressure = PressurePhase::default();
    let mut failure = None;

    loop {
        if started.elapsed() >= duration {
            break;
        }
        match session.try_frame() {
            Ok(Some(frame)) => {
                consumed += 1;
                if let Some(previous) = previous
                    && frame.timestamp.0 <= previous
                {
                    monotonic = false;
                    decreasing.get_or_insert((previous, frame.timestamp.0));
                }
                previous = Some(frame.timestamp.0);
                first_media.get_or_insert(frame.timestamp.0);
                last_media = Some(frame.timestamp.0);

                if pressure.released && pressure.resumed_after.is_none() {
                    pressure.resumed_after = Some(consumed);
                }
                if stall_consumer_ms > 0 && !pressure.released {
                    pressure.held.push(frame.surface);
                    if pressure.held.len() >= crate::surface::SLOT_COUNT {
                        pressure.started.get_or_insert(Instant::now());
                    }
                }
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(1)),
            Err(error) => {
                failure = Some(error);
                break;
            }
        }

        let pressure_elapsed = pressure.started.map(|held_at| held_at.elapsed());
        if let Some(elapsed) = pressure_elapsed
            && !pressure.released
            && elapsed >= stall
        {
            // Timed pressure ends here: the held leases are released so the run can show
            // that capture resumes, while a separate phase below keeps leases through
            // stop. Retention beyond this instant is never claimed.
            pressure.held_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
            pressure.exhausted_during_pressure = session
                .shared
                .counters
                .pool_exhausted
                .load(Ordering::Relaxed);
            pressure.held.clear();
            pressure.released = true;
            pressure.resume_deadline = Some(Instant::now() + PROBE_RESUME_WINDOW);
        }
        if let Some(deadline) = pressure.resume_deadline
            && pressure.resumed_within_window.is_none()
            && Instant::now() >= deadline
        {
            // Reporting "did not resume" is honest; claiming it resumed is not.
            pressure.resumed_within_window = Some(pressure.resumed_after.is_some());
        }
    }

    // Phase 2: leases retained across stop.
    let mut held: Vec<WindowsSurface> = Vec::new();
    if failure.is_none() {
        let deadline = Instant::now() + PROBE_RETAIN_WINDOW;
        while held.len() < crate::surface::SLOT_COUNT && Instant::now() < deadline {
            match session.try_frame() {
                Ok(Some(frame)) => {
                    consumed += 1;
                    held.push(frame.surface);
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(1)),
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
    }
    let held_before_stop = held.len();
    let stop_result = session.stop();
    let slots_after_stop = session.shared.pool.counts();
    let payload_after_stop = session.shared.lifetime.live();
    let payload_created = session.shared.lifetime.created();
    let diagnostics = session.diagnostics()?;
    let held_states: Vec<&'static str> =
        held.iter().map(|lease| lease.slot_state().name()).collect();
    drop(held);
    let slots_after_release = session.shared.pool.counts();
    let payload_after_release = session.shared.lifetime.live();

    let elapsed = started.elapsed();
    let elapsed_seconds = elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
    let counters = session.shared.counters.snapshot();
    let post_stop_queue = SharedState::lock(&session.shared.ready)?.stats();
    let post_stop_slots = session.shared.pool.counts();
    let identities = counters.identities(post_stop_queue, post_stop_slots);

    let mut report = String::new();
    report.push_str("Frame Windows display capture probe (FRAME-0103-R1)\n");
    report.push_str(
        "Read-only proof: it consumes captured surfaces, writes no file and creates no recording.\n",
    );
    report.push_str(&format!(
        "requested: display_id={} seconds={} stall_consumer_ms={} verify_pixels={}\n",
        display_id.0, seconds, stall_consumer_ms, verify_pixels
    ));
    report.push_str(&format!(
        "result: consumed={} in {:.3} s ({:.2}/s) monotonic_timestamps={} source_nominal_hz={:?}\n",
        consumed,
        elapsed_seconds,
        consumed as f64 / elapsed_seconds,
        monotonic,
        source_hz
    ));
    if let Some((previous, observed)) = decreasing {
        report.push_str(&format!(
            "monotonicity violation: mapped timestamp moved from {previous} ns to {observed} ns\n"
        ));
    }
    report.push_str(&format!(
        "desktop_update_observations={} accepted_copies={} published={} updates/s={:.2} published/s={:.2}\n",
        counters.updates_with_image,
        counters.copy_submissions,
        counters.ready_published,
        counters.updates_with_image as f64 / elapsed_seconds,
        counters.ready_published as f64 / elapsed_seconds
    ));
    report.push_str(&format!(
        "timestamps: first_media_ns={:?} last_media_ns={:?} span_ns={:?}\n",
        first_media,
        last_media,
        match (first_media, last_media) {
            (Some(first), Some(last)) => Some(last.saturating_sub(first)),
            _ => None,
        }
    ));
    if verify_pixels {
        report.push_str(&format!(
            "verified_content: readback_samples={} changed_samples={} bounded_sampling_rate_hz={:.2} (sampled every {} ms, at most {} samples; not a frame-rate measurement)\n",
            counters.readback_copies,
            counters.readback_changed,
            counters.readback_copies as f64 / elapsed_seconds,
            worker::READBACK_INTERVAL.as_millis(),
            worker::MAX_READBACK_SAMPLES
        ));
        if let Some(readback) = *SharedState::lock(&session.shared.readback)? {
            report.push_str(&format!(
                "verified_readback: row_pitch={} bytes={} valid_row_bytes={} hashed_bytes={} padded={} expected_min_pitch={}\n",
                readback.row_pitch,
                readback.bytes,
                readback.valid_row_bytes,
                readback.hash_bytes,
                readback.padded,
                session.shared.width() * 4
            ));
            report.push_str(&format!(
                "verified_pixels (BGRA, sampled grid, one point per {} px column band at mid-height):\n",
                session.shared.width() / 8
            ));
            for sample in readback.samples.iter() {
                report.push_str(&format!(
                    "  at ({},{}) bgra=[{},{},{},{}]\n",
                    sample.x,
                    sample.y,
                    sample.bgra[0],
                    sample.bgra[1],
                    sample.bgra[2],
                    sample.bgra[3]
                ));
            }
        }
    } else {
        report.push_str("verified_content: not requested (no readback in ordinary capture)\n");
    }
    report.push_str(&format!(
        "pressure_phase: requested_hold_ms={} held_ms={} leases_held={} exceeded_before_release={} resumed_after_release={:?} resumed_within_{}ms={:?} (retention ends at release; leases held through stop are reported below)\n",
        stall_consumer_ms,
        pressure.held_ms,
        crate::surface::SLOT_COUNT,
        pressure.exhausted_during_pressure,
        pressure.resumed_after,
        PROBE_RESUME_WINDOW.as_millis(),
        pressure.resumed_within_window
    ));
    report.push_str(&format!(
        "leases_through_stop: held_before_stop={} lease_states={:?} pool_after_stop(free={} copy_pending={} ready_queued={} consumer_owned={}) pool_after_release(free={})\n",
        held_before_stop,
        held_states,
        slots_after_stop.free,
        slots_after_stop.copy_pending,
        slots_after_stop.ready_queued,
        slots_after_stop.consumer_owned,
        slots_after_release.free
    ));
    report.push_str(&format!(
        "payload_owners: created={} live_after_stop={} live_after_lease_release={} (destruction evidence, not slot labels)\n",
        payload_created, payload_after_stop, payload_after_release
    ));
    report.push_str(&format!(
        "stop_result={:?} teardown_ms={:.3}\n",
        stop_result
            .as_ref()
            .err()
            .map(|error| error.message.clone()),
        counters.teardown_nanos as f64 / 1_000_000.0
    ));
    report.push('\n');
    report.push_str(&diagnostics);

    // The probe's own acceptance checks. Printing a violated invariant alongside a
    // success exit code would be a false pass.
    if let Some(error) = failure {
        return Err(RecorderError {
            message: format!("{report}\nProbe failure: {}", error.message),
            ..error
        });
    }
    if !monotonic {
        return Err(RecorderError {
            message: format!(
                "{report}\nProbe failure: accepted timestamps were not strictly increasing."
            ),
            ..internal_error(
                "Accepted capture timestamps were not strictly increasing.".to_string(),
            )
        });
    }
    if !identities.all() {
        return Err(RecorderError {
            message: format!(
                "{report}\nProbe failure: post-stop counters did not reconcile."
            ),
            ..internal_error(
                "Post-stop capture counters did not reconcile, so the run's accounting is not trustworthy."
                    .to_string(),
            )
        });
    }
    Ok(report)
}
