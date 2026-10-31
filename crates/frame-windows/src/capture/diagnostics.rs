//! Bounded capture accounting and the human-readable diagnostics report
//! (FRAME-0103, ADR 0005).
//!
//! Counters are separate on purpose: acquisition, desktop-update, pointer-only,
//! repeated, pre-epoch, upstream-accumulated, copy, drop, dequeue and shutdown
//! outcomes are different facts and must not be merged. The report is bounded text:
//! counters, the first and last observation, at most [`MAX_DETAILED_SAMPLES`] detailed
//! samples and the private clock evidence. There is no per-frame growing log, and no
//! IPC field is added.
//!
//! The three accounting identities that the report re-checks are:
//!
//! ```text
//! acquire_success               = updates_with_image + pointer_only + replay_samples
//! updates_with_image+pointer_only+replay_samples = repeats + stale_visual + pre_epoch
//!                               + discontinuities + unmappable
//!                               + pool_exhausted + copy_submissions
//! copy_submissions              = copy_completions + copy_failures + slots.copy_pending
//!                               + shutdown_discarded_pending
//! copy_completions              = queue.depth + dequeued + ready_evictions
//!                               + worker_contention_drops + shutdown_discarded_queued
//!                               + discarded_on_verification_failure
//!                               + discarded_on_queue_poison
//! ```
//!
//! A live snapshot is not atomic, so a mismatch is reported as such instead of being
//! presented as a guarantee; post-stop counters are expected to reconcile.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use frame_contracts::RecorderError;
use frame_metrics::QueueStats;

use crate::capture::PreviewSlotState;
use crate::preview::{PreviewConfig, PreviewCounterSnapshot};

use crate::surface::SlotCounts;

/// Detailed samples retained for diagnostics (ADR 0005: at most 64).
pub(crate) const MAX_DETAILED_SAMPLES: usize = 64;

/// Pixel evidence points reported by opt-in readback verification. One point per
/// equal-width column band at mid-height, so a known 8-band pattern can be checked by
/// coordinate as well as by digest.
pub(crate) const READBACK_PIXEL_SAMPLES: usize = 8;

/// One sampled BGRA quadruple from the verification readback.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PixelSample {
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) bgra: [u8; 4],
}

/// How one session mutex could be read without blocking.
///
/// `Contended` and `Poisoned` are explicit outcomes: a diagnostic snapshot never waits
/// for a lock and never silently substitutes a default value.
pub(crate) enum Snapshot<T> {
    Value(T),
    Contended,
    Poisoned,
}

/// Three-decimal millisecond formatting for durations.
fn ms(nanoseconds: u64) -> f64 {
    nanoseconds as f64 / 1_000_000.0
}

/// A read-only, typed view of the capture counters one running session reports.
///
/// One field per accounting fact, and no combined "total loss": the categories below are
/// the distinct outcomes of the identities documented at the top of this module, so adding
/// two of them into a single figure is exactly what double-counts a loss. In particular
///
/// - [`Self::upstream_accumulated_updates`] is an upstream accumulation counter, not a
///   Frame drop and not an input to any identity here;
/// - [`Self::protected_content_masked`] marks an update that is *also* counted in
///   [`Self::updates_with_image`]: it is a property of that update, not another frame, and
///   it is neither a copy nor a delivery failure;
/// - [`Self::consumer_contention`] is a short `try_lock` miss by the consumer, not a
///   dropped frame;
/// - the opt-in readback sub-account is deliberately absent: its outcomes already appear
///   in the delivery identity, so exporting both would invite counting them twice.
///
/// The counters are cumulative for the whole session. No value here is derived, smoothed
/// or estimated: a fact the session did not observe is a counter that never moved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CaptureCounters {
    /// Desktop updates that carried an image.
    pub updates_with_image: u64,
    /// QPC-timed samples of retained pixels for replay, not native presentations.
    pub replay_samples: u64,
    /// Distinct, increasing native desktop presentation timestamps mapped after the
    /// capture epoch, observed before pool admission. Cursor-only events, repeated
    /// timestamps and `AccumulatedFrames` estimates are excluded. This is a native
    /// presentation identity count, not proof of distinct pixel content or source FPS
    /// that the operating system did not deliver to capture.
    pub native_desktop_presentations: u64,
    /// Completed GPU copies of distinct desktop presentation identities; no cursor-only updates.
    pub captured_desktop_presentations: u64,
    /// Copy attempts that claimed a pool slot.
    pub copy_submissions: u64,
    /// Copies that completed and released a filled pool slot.
    pub copy_completions: u64,
    /// Completed frames the consumer took from the ready queue.
    pub dequeued: u64,
    /// Updates whose image was a repeat of the previous one: not copied.
    pub repeats: u64,
    /// Source updates older than the latest delivered combined visual update.
    pub stale_visual: u64,
    /// Updates whose timestamp was before the session epoch: not copied.
    pub pre_epoch: u64,
    /// Updates that followed a desktop discontinuity: not copied.
    pub discontinuities: u64,
    /// Updates whose desktop resource could not be mapped: not copied.
    pub unmappable: u64,
    /// Updates dropped because all four pool slots were occupied: not copied.
    pub pool_exhausted: u64,
    /// Copy attempts that failed.
    pub copy_failures: u64,
    /// Completed frames evicted from the ready queue by a newer frame.
    pub ready_evictions: u64,
    /// Completed frames the worker discarded because it could not take the ready queue
    /// without blocking.
    pub worker_contention_drops: u64,
    /// Slots reclaimed because the session was cancelled while a copy was still pending.
    pub stop_discarded_pending: u64,
    /// Queued frames discarded by an explicit stop.
    pub stop_discarded_queued: u64,
    /// Completed frames discarded because opt-in pixel verification failed.
    pub verification_discarded: u64,
    /// Completed frames discarded because the ready queue was poisoned.
    pub poison_discarded: u64,
    /// Completed frames discarded because the session was cancelled during verification.
    pub cancellation_discarded: u64,
    /// Slots reclaimed by a worker unwind out of a copy submission.
    pub unwind_discarded_pending: u64,
    /// Slots reclaimed by a worker unwind out of verification or publication.
    pub unwind_discarded_completed: u64,
    /// Upstream accumulated desktop updates. An upstream counter, never a Frame drop.
    pub upstream_accumulated_updates: u64,
    /// Updates the operating system reported as protected content. This is also counted in
    /// [`Self::updates_with_image`]: it marks that update, it does not add one.
    pub protected_content_masked: u64,
    /// Consumer `try_lock` misses on the ready queue: contention, not a dropped frame.
    pub consumer_contention: u64,
}

impl CaptureCounters {
    /// Copies the session's atomics into the typed view.
    pub(crate) fn from_snapshot(snapshot: &CounterSnapshot) -> Self {
        Self {
            updates_with_image: snapshot.updates_with_image,
            replay_samples: snapshot.replay_samples,
            native_desktop_presentations: snapshot.native_desktop_presentations,
            captured_desktop_presentations: snapshot.captured_desktop_presentations,
            copy_submissions: snapshot.copy_submissions,
            copy_completions: snapshot.copy_completions,
            dequeued: snapshot.dequeued,
            repeats: snapshot.repeats,
            stale_visual: snapshot.stale_visual,
            pre_epoch: snapshot.pre_epoch,
            discontinuities: snapshot.discontinuities,
            unmappable: snapshot.unmappable,
            pool_exhausted: snapshot.pool_exhausted,
            copy_failures: snapshot.copy_failures,
            ready_evictions: snapshot.ready_evictions,
            worker_contention_drops: snapshot.worker_contention_drops,
            stop_discarded_pending: snapshot.shutdown_discarded_pending,
            stop_discarded_queued: snapshot.shutdown_discarded_queued,
            verification_discarded: snapshot.discarded_on_verification_failure,
            poison_discarded: snapshot.discarded_on_queue_poison,
            cancellation_discarded: snapshot.discarded_on_cancellation,
            unwind_discarded_pending: snapshot.discarded_on_unwind_pending,
            unwind_discarded_completed: snapshot.discarded_on_unwind_completed,
            upstream_accumulated_updates: snapshot.upstream_accumulated_updates,
            protected_content_masked: snapshot.protected_content,
            consumer_contention: snapshot.consumer_contention,
        }
    }
}

/// One read-only snapshot of a capture session's counters, queue gauge and nominal rate.
///
/// This is a live snapshot of separate facts, not one atomic global snapshot of the
/// session: while the worker runs, two fields can come from two different instants. Taking
/// it never blocks — the atomics are read directly and the ready queue is entered with
/// `try_lock` only — and [`Self::ready_queue`] is `None` when that queue could not be read
/// without blocking or was poisoned. No substitute value is invented for it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CaptureSessionStats {
    /// The session number this snapshot belongs to, so two sessions cannot be confused.
    pub session_id: u64,
    /// Whether this session's `stop` already returned.
    pub stopped: bool,
    /// Whether the capture worker has finished.
    pub worker_finished: bool,
    /// The nominal mode rate from this session's own start observation, when it was known.
    /// It is a hardware mode value, never a capture or encode measurement.
    pub nominal_source_hz: Option<f64>,
    /// The ready-queue gauge: depth, capacity, high water and evictions.
    pub ready_queue: Option<QueueStats>,
    pub counters: CaptureCounters,
}

/// Separately maintained capture counters.
#[derive(Default)]
pub(crate) struct Counters {
    pub(crate) acquire_success: AtomicU64,
    pub(crate) acquire_timeouts: AtomicU64,
    pub(crate) acquire_failures: AtomicU64,
    pub(crate) updates_with_image: AtomicU64,
    pub(crate) replay_samples: AtomicU64,
    pub(crate) native_desktop_presentations: AtomicU64,
    pub(crate) captured_desktop_presentations: AtomicU64,
    pub(crate) pointer_only: AtomicU64,
    pub(crate) protected_content: AtomicU64,
    pub(crate) repeats: AtomicU64,
    pub(crate) stale_visual: AtomicU64,
    pub(crate) pre_epoch: AtomicU64,
    pub(crate) discontinuities: AtomicU64,
    pub(crate) unmappable: AtomicU64,
    pub(crate) upstream_accumulated_updates: AtomicU64,
    pub(crate) copy_submissions: AtomicU64,
    pub(crate) copy_completions: AtomicU64,
    pub(crate) copy_failures: AtomicU64,
    pub(crate) pool_exhausted: AtomicU64,
    pub(crate) ready_published: AtomicU64,
    pub(crate) ready_evictions: AtomicU64,
    pub(crate) worker_contention_drops: AtomicU64,
    pub(crate) consumer_contention: AtomicU64,
    pub(crate) dequeued: AtomicU64,
    pub(crate) shutdown_discarded_pending: AtomicU64,
    pub(crate) shutdown_discarded_queued: AtomicU64,
    /// Completed copies whose slot was returned because opt-in verification failed.
    pub(crate) discarded_on_verification_failure: AtomicU64,
    /// Completed copies whose slot was returned because the ready queue was poisoned.
    pub(crate) discarded_on_queue_poison: AtomicU64,
    /// Completed copies whose slot was returned because the session was cancelled
    /// during verification. Cancellation is a normal stop, not a failure.
    pub(crate) discarded_on_cancellation: AtomicU64,
    /// Slots reclaimed by unwinding out of a copy submission.
    pub(crate) discarded_on_unwind_pending: AtomicU64,
    /// Slots reclaimed by unwinding out of verification or publication.
    pub(crate) discarded_on_unwind_completed: AtomicU64,
    /// Diagnostic samples the worker skipped rather than waiting for the sample log.
    pub(crate) diagnostic_samples_skipped: AtomicU64,
    /// Readback evidence snapshots skipped because the evidence lock was contended.
    pub(crate) readback_evidence_skipped: AtomicU64,
    /// Opt-in readback runs that stopped because the session was cancelled.
    pub(crate) readback_cancelled: AtomicU64,
    pub(crate) readback_copies: AtomicU64,
    pub(crate) readback_failures: AtomicU64,
    pub(crate) readback_changed: AtomicU64,
    /// Bounded `Map` polls that found the staging resource still busy.
    pub(crate) readback_map_polls: AtomicU64,
    /// Exactly-once unmaps after a successful map.
    pub(crate) readback_unmaps: AtomicU64,
    pub(crate) protected_content_seen: AtomicBool,
    pub(crate) native_failures: AtomicU64,
    pub(crate) teardown_nanos: AtomicU64,
}

impl Counters {
    /// Increments one counter by one and returns the new value.
    pub(crate) fn bump(counter: &AtomicU64) -> u64 {
        counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub(crate) fn snapshot(&self) -> CounterSnapshot {
        CounterSnapshot {
            acquire_success: self.acquire_success.load(Ordering::Relaxed),
            acquire_timeouts: self.acquire_timeouts.load(Ordering::Relaxed),
            acquire_failures: self.acquire_failures.load(Ordering::Relaxed),
            updates_with_image: self.updates_with_image.load(Ordering::Relaxed),
            replay_samples: self.replay_samples.load(Ordering::Relaxed),
            native_desktop_presentations: self.native_desktop_presentations.load(Ordering::Relaxed),
            captured_desktop_presentations: self
                .captured_desktop_presentations
                .load(Ordering::Relaxed),
            pointer_only: self.pointer_only.load(Ordering::Relaxed),
            protected_content: self.protected_content.load(Ordering::Relaxed),
            repeats: self.repeats.load(Ordering::Relaxed),
            stale_visual: self.stale_visual.load(Ordering::Relaxed),
            pre_epoch: self.pre_epoch.load(Ordering::Relaxed),
            discontinuities: self.discontinuities.load(Ordering::Relaxed),
            unmappable: self.unmappable.load(Ordering::Relaxed),
            upstream_accumulated_updates: self.upstream_accumulated_updates.load(Ordering::Relaxed),
            copy_submissions: self.copy_submissions.load(Ordering::Relaxed),
            copy_completions: self.copy_completions.load(Ordering::Relaxed),
            copy_failures: self.copy_failures.load(Ordering::Relaxed),
            pool_exhausted: self.pool_exhausted.load(Ordering::Relaxed),
            ready_published: self.ready_published.load(Ordering::Relaxed),
            ready_evictions: self.ready_evictions.load(Ordering::Relaxed),
            worker_contention_drops: self.worker_contention_drops.load(Ordering::Relaxed),
            consumer_contention: self.consumer_contention.load(Ordering::Relaxed),
            dequeued: self.dequeued.load(Ordering::Relaxed),
            shutdown_discarded_pending: self.shutdown_discarded_pending.load(Ordering::Relaxed),
            shutdown_discarded_queued: self.shutdown_discarded_queued.load(Ordering::Relaxed),
            discarded_on_verification_failure: self
                .discarded_on_verification_failure
                .load(Ordering::Relaxed),
            discarded_on_queue_poison: self.discarded_on_queue_poison.load(Ordering::Relaxed),
            discarded_on_cancellation: self.discarded_on_cancellation.load(Ordering::Relaxed),
            discarded_on_unwind_pending: self.discarded_on_unwind_pending.load(Ordering::Relaxed),
            discarded_on_unwind_completed: self
                .discarded_on_unwind_completed
                .load(Ordering::Relaxed),
            diagnostic_samples_skipped: self.diagnostic_samples_skipped.load(Ordering::Relaxed),
            readback_evidence_skipped: self.readback_evidence_skipped.load(Ordering::Relaxed),
            readback_cancelled: self.readback_cancelled.load(Ordering::Relaxed),
            readback_copies: self.readback_copies.load(Ordering::Relaxed),
            readback_failures: self.readback_failures.load(Ordering::Relaxed),
            readback_changed: self.readback_changed.load(Ordering::Relaxed),
            readback_map_polls: self.readback_map_polls.load(Ordering::Relaxed),
            readback_unmaps: self.readback_unmaps.load(Ordering::Relaxed),
            native_failures: self.native_failures.load(Ordering::Relaxed),
            teardown_nanos: self.teardown_nanos.load(Ordering::Relaxed),
            protected_content_seen: self.protected_content_seen.load(Ordering::Relaxed),
        }
    }
}

/// A plain copy of the counters, so the report renders one consistent snapshot
/// instead of re-reading atomics while it formats.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CounterSnapshot {
    pub(crate) acquire_success: u64,
    pub(crate) acquire_timeouts: u64,
    pub(crate) acquire_failures: u64,
    pub(crate) updates_with_image: u64,
    pub(crate) replay_samples: u64,
    pub(crate) native_desktop_presentations: u64,
    pub(crate) captured_desktop_presentations: u64,
    pub(crate) pointer_only: u64,
    pub(crate) protected_content: u64,
    pub(crate) repeats: u64,
    pub(crate) stale_visual: u64,
    pub(crate) pre_epoch: u64,
    pub(crate) discontinuities: u64,
    pub(crate) unmappable: u64,
    pub(crate) upstream_accumulated_updates: u64,
    pub(crate) copy_submissions: u64,
    pub(crate) copy_completions: u64,
    pub(crate) copy_failures: u64,
    pub(crate) pool_exhausted: u64,
    pub(crate) ready_published: u64,
    pub(crate) ready_evictions: u64,
    pub(crate) worker_contention_drops: u64,
    pub(crate) consumer_contention: u64,
    pub(crate) dequeued: u64,
    pub(crate) shutdown_discarded_pending: u64,
    pub(crate) shutdown_discarded_queued: u64,
    pub(crate) discarded_on_verification_failure: u64,
    pub(crate) discarded_on_queue_poison: u64,
    pub(crate) discarded_on_cancellation: u64,
    pub(crate) discarded_on_unwind_pending: u64,
    pub(crate) discarded_on_unwind_completed: u64,
    pub(crate) diagnostic_samples_skipped: u64,
    pub(crate) readback_evidence_skipped: u64,
    pub(crate) readback_cancelled: u64,
    pub(crate) readback_copies: u64,
    pub(crate) readback_failures: u64,
    pub(crate) readback_changed: u64,
    pub(crate) readback_map_polls: u64,
    pub(crate) readback_unmaps: u64,
    pub(crate) native_failures: u64,
    pub(crate) teardown_nanos: u64,
    pub(crate) protected_content_seen: bool,
}

impl CounterSnapshot {
    pub(crate) fn shutdown_discards(&self) -> u64 {
        self.shutdown_discarded_pending + self.shutdown_discarded_queued
    }

    /// The identities from the module documentation, evaluated for this snapshot.
    pub(crate) fn identities(&self, queue: QueueStats, slots: SlotCounts) -> Identities {
        Identities {
            acquisition: self.acquire_success
                == self.updates_with_image + self.pointer_only + self.replay_samples,
            images: self.updates_with_image + self.pointer_only + self.replay_samples
                == self.repeats
                    + self.stale_visual
                    + self.pre_epoch
                    + self.discontinuities
                    + self.unmappable
                    + self.pool_exhausted
                    + self.copy_submissions,
            copies: self.copy_submissions
                == self.copy_completions
                    + self.copy_failures
                    + slots.copy_pending as u64
                    + self.shutdown_discarded_pending
                    + self.discarded_on_unwind_pending,
            delivery: self.copy_completions
                == queue.depth as u64
                    + self.dequeued
                    + self.ready_evictions
                    + self.worker_contention_drops
                    + self.shutdown_discarded_queued
                    + self.discarded_on_verification_failure
                    + self.discarded_on_queue_poison
                    + self.discarded_on_cancellation
                    + self.discarded_on_unwind_completed,
            slots: slots.occupied() + slots.free == crate::surface::SLOT_COUNT,
        }
    }
}

/// Which accounting identities hold for one snapshot.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Identities {
    pub(crate) acquisition: bool,
    pub(crate) images: bool,
    pub(crate) copies: bool,
    pub(crate) delivery: bool,
    pub(crate) slots: bool,
}

impl Identities {
    pub(crate) fn all(&self) -> bool {
        self.acquisition && self.images && self.copies && self.delivery && self.slots
    }
}

/// One accepted desktop update, kept for the bounded sample log.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FrameSample {
    pub(crate) sequence: u64,
    pub(crate) native_ticks: i64,
    pub(crate) media_nanos: u64,
    pub(crate) accumulated_frames: u32,
    /// `Some(changed)` only when opt-in readback verification ran for this sample.
    pub(crate) verified_content_changed: Option<bool>,
}

/// First/last observations plus at most [`MAX_DETAILED_SAMPLES`] detailed samples.
#[derive(Default, Clone)]
pub(crate) struct ObservationLog {
    first: Option<FrameSample>,
    last: Option<FrameSample>,
    samples: Vec<FrameSample>,
}

impl ObservationLog {
    pub(crate) fn record(&mut self, sample: FrameSample) {
        if self.first.is_none() {
            self.first = Some(sample);
        }
        self.last = Some(sample);
        if self.samples.len() < MAX_DETAILED_SAMPLES {
            self.samples.push(sample);
        }
    }

    pub(crate) fn first(&self) -> Option<FrameSample> {
        self.first
    }

    pub(crate) fn last(&self) -> Option<FrameSample> {
        self.last
    }

    pub(crate) fn samples(&self) -> &[FrameSample] {
        &self.samples
    }

    pub(crate) fn truncated(&self) -> bool {
        self.samples.len() >= MAX_DETAILED_SAMPLES
    }
}

/// Private clock alignment evidence: the raw bracket, QPC and frequency stay here
/// and are never exported as a native model.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClockEvidence {
    pub(crate) qpc: u64,
    pub(crate) ticks_per_second: u64,
    pub(crate) before_ns: u64,
    pub(crate) after_ns: u64,
    pub(crate) bracket_ns: u64,
    pub(crate) samples_taken: u32,
    pub(crate) uncertainty_ns: u64,
    pub(crate) anchor_media_ns: u64,
}

/// Native description recorded at preparation, so diagnostics never hold COM types.
#[derive(Clone, Debug)]
pub(crate) struct NativeDescription {
    pub(crate) api: String,
    pub(crate) adapter_description: String,
    pub(crate) output_gdi_name: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixel_format: String,
    pub(crate) rotation_identity: bool,
    pub(crate) desktop_image_in_system_memory: bool,
    /// Whether D3D11 immediate-context multithread protection was enabled and verified on
    /// the capture device before it was exposed to the encoder (ADR 0006 R2 addendum).
    pub(crate) context_multithread_protected: bool,
    pub(crate) staging_bytes: u64,
}

/// Opt-in readback evidence. The digest covers every valid BGRA byte of every row and
/// excludes row padding; the sampled quadruples make BGRA order and coordinates
/// checkable against a known pattern.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReadbackEvidence {
    pub(crate) row_pitch: u32,
    pub(crate) bytes: usize,
    /// `width * 4`: the valid pixel bytes of one row, which is what the digest hashes.
    pub(crate) valid_row_bytes: usize,
    /// Total bytes hashed: `valid_row_bytes * height`.
    pub(crate) hash_bytes: usize,
    pub(crate) padded: bool,
    pub(crate) samples: [PixelSample; READBACK_PIXEL_SAMPLES],
}

/// Everything the report renders, already snapshotted by the caller.
pub(crate) struct ReportInput<'a> {
    pub(crate) session_id: u64,
    pub(crate) stopped: bool,
    pub(crate) worker_finished: bool,
    pub(crate) native: Option<&'a NativeDescription>,
    pub(crate) counters: CounterSnapshot,
    pub(crate) queue: QueueStats,
    pub(crate) queue_available: bool,
    pub(crate) slots: SlotCounts,
    pub(crate) payload_bytes: u64,
    pub(crate) clock: Option<ClockEvidence>,
    pub(crate) log: &'a ObservationLog,
    pub(crate) terminal: Option<&'a RecorderError>,
    pub(crate) verify_pixels: bool,
    pub(crate) consumer_held: usize,
    pub(crate) readback: Option<ReadbackEvidence>,
    /// Sections that could not be read without blocking, as `name(reason)`.
    pub(crate) unavailable: Vec<&'static str>,
    /// Live and created Frame-owned payload owners (destruction evidence).
    pub(crate) lifetime_live: i64,
    pub(crate) lifetime_created: u64,
    /// Whether the session still holds its own payload reference.
    pub(crate) session_payload_held: bool,
    /// Preflighted owned payload for this session, including the staging texture.
    pub(crate) payload_plan_bytes: u64,
    /// Nominal source rate from this session's own start observation.
    pub(crate) nominal_source_hz: Option<f64>,
    pub(crate) preview_attached: bool,
    pub(crate) preview_config: Option<PreviewConfig>,
    pub(crate) preview_generation: Option<u64>,
    pub(crate) preview_subscribers: Option<i64>,
    pub(crate) preview_enabled: Option<bool>,
    pub(crate) preview_counters: Option<PreviewCounterSnapshot>,
    pub(crate) preview: Option<PreviewSlotState>,
}

/// Renders the bounded human-readable diagnostics report.
pub(crate) fn render(input: &ReportInput<'_>) -> String {
    let counters = &input.counters;
    let identities = counters.identities(input.queue, input.slots);
    let mut report = String::new();
    report.push_str(&format!(
        "Frame Windows display capture diagnostics (FRAME-0103, session {})\n",
        input.session_id
    ));
    report.push_str(
        "This is a capture proof, not a recording: it writes no file and creates no artifact.\n",
    );
    report.push_str(
        "Counters below are a live snapshot: they are not an atomic global snapshot while the\nworker may still be running, and only a post-stop snapshot is expected to reconcile.\n",
    );
    report.push_str(&format!(
        "state: {}\n",
        if input.worker_finished {
            if input.stopped {
                "stopped (worker joined)"
            } else {
                "worker finished, session not stopped"
            }
        } else {
            "running"
        }
    ));

    match input.native {
        Some(native) => report.push_str(&format!(
            "native: {} {}x{} {} adapter={:?} output={} rotation_identity={} system_memory={} context_multithread_protected={}\n",
            native.api,
            native.width,
            native.height,
            native.pixel_format,
            native.adapter_description,
            native.output_gdi_name,
            native.rotation_identity,
            native.desktop_image_in_system_memory,
        native.context_multithread_protected
        )),
        None => report.push_str("native: not prepared\n"),
    }
    report.push_str(&format!(
        "source: nominal_rate_hz={:?} (from this session's start observation; not a capture or encode rate)\n",
        input.nominal_source_hz
    ));
    report.push_str(&format!(
        "payload: owned={} preflighted={} budget={} staging={} bytes owners_created={} owners_live={} session_reference_held={}\n",
        input.payload_bytes,
        input.payload_plan_bytes,
        crate::surface::MAX_PAYLOAD_BYTES,
        input.native.map(|native| native.staging_bytes).unwrap_or(0),
        input.lifetime_created,
        input.lifetime_live,
        input.session_payload_held
    ));
    report.push_str(&format!(
        "slots: free={} copy_pending={} ready_queued={} consumer_owned={} of {} (leases held by this consumer at snapshot: {})\n",
        input.slots.free,
        input.slots.copy_pending,
        input.slots.ready_queued,
        input.slots.consumer_owned,
        crate::surface::SLOT_COUNT,
        input.consumer_held
    ));
    if input.queue_available {
        report.push_str(&format!(
            "queue: depth={} capacity={} high_water={} evicted={}\n",
            input.queue.depth, input.queue.capacity, input.queue.high_water, input.queue.dropped
        ));
    } else {
        report.push_str(
            "queue: unavailable at this snapshot; see the contention note below instead of a value\n",
        );
    }
    if input.unavailable.is_empty() {
        report.push_str("snapshot reads: every section was read without blocking\n");
    } else {
        report.push_str(&format!(
            "snapshot reads: {:?} could not be read without blocking or was poisoned; no value is substituted for them\n",
            input.unavailable
        ));
    }

    report.push_str("acquisition:\n");
    report.push_str(&format!(
        "  success={} timeouts={} failures={} native_failures={}\n",
        counters.acquire_success,
        counters.acquire_timeouts,
        counters.acquire_failures,
        counters.native_failures
    ));
    report.push_str(&format!(
        "  updates_with_image={} pointer_only={} protected_content_masked={} protected_seen={}\n",
        counters.updates_with_image,
        counters.pointer_only,
        counters.protected_content,
        counters.protected_content_seen
    ));
    report.push_str(&format!(
        "  native_desktop_presentations={} captured_desktop_presentations={} (distinct native timestamps; cursor-only excluded; not pixel-content uniqueness)\n",
        counters.native_desktop_presentations, counters.captured_desktop_presentations
    ));
    report.push_str(&format!(
        "  replay_samples={} (retained pixels; not native presentations)\n",
        counters.replay_samples
    ));
    report.push_str(&format!(
        "  repeats={} stale_visual={} pre_epoch={} discontinuities={} unmappable={} upstream_accumulated_updates={}\n",
        counters.repeats,
        counters.stale_visual,
        counters.pre_epoch,
        counters.discontinuities,
        counters.unmappable,
        counters.upstream_accumulated_updates
    ));
    report.push_str("copies and delivery:\n");
    report.push_str(&format!(
        "  copy_submissions={} copy_completions={} copy_failures={} pool_exhausted={}\n",
        counters.copy_submissions,
        counters.copy_completions,
        counters.copy_failures,
        counters.pool_exhausted
    ));
    report.push_str(&format!(
        "  ready_published={} ready_evictions={} worker_contention_drops={} consumer_contention={}\n",
        counters.ready_published,
        counters.ready_evictions,
        counters.worker_contention_drops,
        counters.consumer_contention
    ));
    report.push_str(&format!(
        "  dequeued={} shutdown_discards={} (pending={} queued={}) discarded_on_verification_failure={} discarded_on_queue_poison={} discarded_on_cancellation={}\n",
        counters.dequeued,
        counters.shutdown_discards(),
        counters.shutdown_discarded_pending,
        counters.shutdown_discarded_queued,
        counters.discarded_on_verification_failure,
        counters.discarded_on_queue_poison,
        counters.discarded_on_cancellation
    ));
    report.push_str("verification (opt-in readback, off in ordinary capture):\n");
    report.push_str(&format!(
        "  enabled={} readback_copies={} readback_changed={} readback_failures={} readback_cancelled={} map_polls={} unmaps={}\n",
        input.verify_pixels,
        counters.readback_copies,
        counters.readback_changed,
        counters.readback_failures,
        counters.readback_cancelled,
        counters.readback_map_polls,
        counters.readback_unmaps
    ));
    if let Some(readback) = input.readback {
        report.push_str(&format!(
            "  row_pitch={} mapped_bytes={} valid_row_bytes={} hashed_bytes={} padded={}\n",
            readback.row_pitch,
            readback.bytes,
            readback.valid_row_bytes,
            readback.hash_bytes,
            readback.padded
        ));
    }
    report.push_str(&format!(
        "diagnostic_samples_skipped={} readback_evidence_skipped={} (the worker never waits for diagnostics)\n",
        counters.diagnostic_samples_skipped, counters.readback_evidence_skipped
    ));
    report.push_str("live preview (FRAME-0208, same capture session):\n");
    match input.preview_config {
        Some(config) => report.push_str(&format!(
            "  requested: max={}x{} min_interval_ms={} attached={} generation={:?} subscribers={:?} publishing={:?}\n",
            config.max_width, config.max_height, config.min_interval.as_millis(),
            input.preview_attached, input.preview_generation, input.preview_subscribers,
            input.preview_enabled
        )),
        None => report.push_str(&format!(
            "  requested: none attached={}\n", input.preview_attached
        )),
    }
    match input.preview_counters {
        Some(preview) => {
            report.push_str(&format!(
                "  work: generated={} published={} replaced_not_taken={} skipped_no_subscriber={} rate_limited={} map_busy={} failures={}\n",
                preview.generated, preview.published, preview.replaced_not_taken,
                preview.skipped_no_subscriber, preview.rate_limited, preview.map_busy,
                preview.failures
            ));
            report.push_str(&format!(
                "  worker_cost: readback={:.3} ms encode={:.3} ms total={:.3} ms\n",
                ms(preview.readback_nanos),
                ms(preview.encode_nanos),
                ms(preview.total_nanos())
            ));
        }
        None => report.push_str("  work: unavailable at this snapshot\n"),
    }
    match input.preview {
        Some(preview) => report.push_str(&format!(
            "  last_image: {}x{} encoded_bytes={} source_media_ns={:?} readback={:.3} ms encode={:.3} ms\n",
            preview.cost.image_width, preview.cost.image_height,
            preview.cost.encoded_bytes, preview.media_nanos,
            ms(preview.cost.readback_nanos), ms(preview.cost.encode_nanos)
        )),
        None => report.push_str("  last_image: none was produced\n"),
    }
    report.push_str(&format!(
        "unwind_discards: pending={} completed={} (slots reclaimed by a worker unwind)\n",
        counters.discarded_on_unwind_pending, counters.discarded_on_unwind_completed
    ));
    report.push_str(&format!("teardown={:.3} ms\n", ms(counters.teardown_nanos)));

    match input.clock {
        Some(clock) => report.push_str(&format!(
            "clock: qpc={} qpf={} before_ns={} after_ns={} bracket_ns={} samples={} anchor_media_ns={} uncertainty_ns={}\n",
            clock.qpc,
            clock.ticks_per_second,
            clock.before_ns,
            clock.after_ns,
            clock.bracket_ns,
            clock.samples_taken,
            clock.anchor_media_ns,
            clock.uncertainty_ns
        )),
        None => report.push_str("clock: not aligned\n"),
    }

    match (input.log.first(), input.log.last()) {
        (Some(first), Some(last)) => report.push_str(&format!(
            "observations: first(seq={} qpc={} media_ns={}) last(seq={} qpc={} media_ns={}) span_ns={}\n",
            first.sequence,
            first.native_ticks,
            first.media_nanos,
            last.sequence,
            last.native_ticks,
            last.media_nanos,
            last.media_nanos.saturating_sub(first.media_nanos)
        )),
        _ => report.push_str("observations: no desktop image was accepted\n"),
    }
    for sample in input.log.samples() {
        report.push_str(&format!(
            "  sample seq={} qpc={} media_ns={} accumulated={} verified_changed={}\n",
            sample.sequence,
            sample.native_ticks,
            sample.media_nanos,
            sample.accumulated_frames,
            match sample.verified_content_changed {
                Some(changed) => changed.to_string(),
                None => "n/a".to_string(),
            }
        ));
    }
    if input.log.truncated() {
        report.push_str(&format!(
            "  (detailed samples capped at {}; the counters above remain complete)\n",
            MAX_DETAILED_SAMPLES
        ));
    }

    match input.terminal {
        Some(error) => report.push_str(&format!(
            "terminal error: code={:?} message={} action={}\n",
            error.code, error.message, error.action
        )),
        None => report.push_str("terminal error: none\n"),
    }

    report.push_str(&format!(
        "reconcile: acquisition={} images={} copies={} delivery={} slots={}\n",
        verdict(identities.acquisition),
        verdict(identities.images),
        verdict(identities.copies),
        verdict(identities.delivery),
        verdict(identities.slots)
    ));
    report.push_str(if identities.all() {
        "reconcile verdict: every checked identity holds for this snapshot.\n"
    } else if input.worker_finished && input.stopped {
        "reconcile verdict: a post-stop snapshot did not satisfy every identity; treat the counters as inconsistent and report it rather than as a measured loss figure.\n"
    } else {
        "reconcile verdict: live snapshots are not atomic, so a mismatch here is expected while the worker may still be running; re-check after stop.\n"
    });
    report
}

fn verdict(holds: bool) -> &'static str {
    if holds { "ok" } else { "mismatch" }
}
