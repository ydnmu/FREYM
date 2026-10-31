// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Windows microphone capture (FRAME-0105, ADR 0007).
//!
//! Real capture of one explicitly selected active capture endpoint, delivered as bounded
//! timestamped interleaved `f32` PCM through the accepted [`frame_audio::AudioSession`]
//! contract. The device mix format is preserved exactly: no resampler, mixer, gain,
//! downmix or fallback device, and never a default or system-mix substitution.
//!
//! Ownership: one fresh joined MTA worker owns COM and every WASAPI call for exactly one
//! source. Nothing native escapes this module. The worker converts each nonempty packet
//! into owned PCM while its native buffer is still acquired, publishes it through a
//! bounded queue, and stops the stream and closes its handles when it ends. `stop` and
//! `Drop` are the same safety net: cancel, wake, join, release.
//!
//! Errors are sticky and take precedence over buffered PCM. Queue overflow, a device-frame
//! gap, a timestamp error or any native failure terminates *this* source visibly; it never
//! silently drops audio and never cancels a sibling source that shares the recording
//! clock.

pub(crate) mod format;
mod lifecycle;
pub(crate) mod native;

#[cfg(test)]
use lifecycle::run_started;
use lifecycle::{CaptureStream, run_started_with_release};

#[cfg(test)]
mod drain_tests;
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod review_regressions;

use format::DeviceFormat;
use frame_audio::{
    AudioCancellation, AudioContext, AudioSession, DrainableAudioSession, PcmBlock, PcmDrainPoll,
};
use frame_clock::{ClockAnchor, MasterClock, MediaTimestamp, sample_timestamp};
use frame_contracts::{
    AudioRoute, AudioSource, AudioSourceId, AudioSourceKind, ErrorCode, RecorderError,
    RecoveryStatus,
};
use frame_metrics::{BoundedQueue, OverflowPolicy};
use std::num::{NonZeroU16, NonZeroU32, NonZeroUsize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

/// The identity prefix ADR 0007 fixes for a Windows capture endpoint.
const MICROPHONE_PREFIX: &str = "wasapi-mic:";

/// Bounded queue and payload limits (ADR 0007).
pub(crate) const MAX_QUEUED_BLOCKS: usize = 32;
pub(crate) const MAX_OWNED_PCM_BYTES: u64 = 32 * 1024 * 1024;
// Partition the payload budget between 32 queued blocks and one conversion/in-flight
// block. Conversion never keeps an old allocation while allocating its replacement.
pub(crate) const MAX_PCM_BLOCK_BYTES: u64 = MAX_OWNED_PCM_BYTES / (MAX_QUEUED_BLOCKS as u64 + 1);
/// The worker waits at most this long for data before re-checking cancellation.
const WAIT_SLICE_MS: u32 = 10;
/// ADR 0007's preparation watchdog between native preparation calls.
const PREPARATION_WATCHDOG: Duration = Duration::from_secs(5);
/// At most this many decoded observations are retained for diagnostics.
const MAX_OBSERVATIONS: usize = 64;

fn internal(message: String, action: &str) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: action.to_string(),
    }
}

fn source_lost(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::SourceLost,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: "Re-select the microphone and start a new session.".into(),
    }
}

fn cancelled_start() -> RecorderError {
    RecorderError {
        code: ErrorCode::InvalidTransition,
        message: "Microphone preparation was cancelled before capture started.".to_string(),
        recovery: RecoveryStatus::NoRecording,
        action: "Start the microphone again when you are ready.".into(),
    }
}

/// Builds the canonical source identity for one endpoint.
fn source_id(endpoint_id: &str) -> AudioSourceId {
    AudioSourceId(format!("{MICROPHONE_PREFIX}{endpoint_id}"))
}

/// Returns the endpoint ID when this identity is a Windows microphone identity.
fn endpoint_of(id: &AudioSourceId) -> Option<&str> {
    id.0.strip_prefix(MICROPHONE_PREFIX)
}

/// Frame-owned payload charged for one block: its allocated capacity, not its length.
fn charged_bytes(block: &PcmBlock) -> u64 {
    (block.samples.capacity() as u64).saturating_mul(size_of::<f32>() as u64)
}

/// One bounded decoded observation, kept only while a session lives.
#[derive(Debug, Clone, Copy)]
struct Observation {
    frames: u32,
    device_position: u64,
    raw_qpc_100ns: u64,
    mapped_ns: u64,
    pcm_timestamp_ns: u64,
    silent: bool,
    discontinuity: bool,
    timestamp_error: bool,
}

/// Diagnostics shared between the worker and its session.
#[derive(Debug, Default)]
struct SharedDiagnostics {
    total_frames: AtomicU64,
    packets: AtomicU64,
    empty_packets: AtomicU64,
    silent_packets: AtomicU64,
    discontinuity_flags: AtomicU64,
    pre_epoch_discarded: AtomicU64,
    queue_overflows: AtomicU64,
    discarded_bytes: AtomicU64,
    held_high_water: AtomicU64,
    observations: Mutex<Vec<Observation>>,
}

impl SharedDiagnostics {
    fn observe(&self, observation: Observation) -> Result<(), RecorderError> {
        let mut list = self.observations.lock().map_err(|_| {
            internal(
                "The microphone observation log was poisoned.".into(),
                "Start a new microphone session.",
            )
        })?;
        if list.len() >= MAX_OBSERVATIONS {
            list.remove(0);
        }
        list.push(observation);
        Ok(())
    }
}

/// Worker-side queue state. The consumer never blocks on it.
pub(crate) struct QueueState {
    blocks: BoundedQueue<PcmBlock>,
    held_bytes: u64,
    closed: bool,
}

impl Default for QueueState {
    fn default() -> Self {
        Self {
            blocks: BoundedQueue::new(
                NonZeroUsize::new(MAX_QUEUED_BLOCKS).unwrap(),
                OverflowPolicy::DropNewest,
            ),
            held_bytes: 0,
            closed: false,
        }
    }
}

impl QueueState {
    fn discard(&mut self) -> u64 {
        let mut discarded = 0;
        while let Some(block) = self.blocks.pop() {
            discarded += charged_bytes(&block);
        }
        self.held_bytes = 0;
        self.closed = true;
        discarded
    }
}

/// The worker's terminal outcome, published once.
pub(crate) enum Terminal {
    /// The stream ended cleanly after a stop request.
    Stopped,
    /// The worker finished a requested graceful drain. Every block it published is
    /// retained for the consumer; the drain itself discards nothing.
    Drained,
    /// The source failed; buffered PCM is discarded and the error is sticky.
    Failed(RecorderError),
}

/// One nonblocking observation of the worker's published terminal state.
enum TerminalPoll {
    /// A sticky failure; it precedes buffered PCM.
    Failed(RecorderError),
    /// The worker finished a requested graceful drain and kept its published PCM.
    Drained,
    /// The worker ended after an abort.
    Ended,
    /// No terminal outcome has been published.
    Unpublished,
    /// A panic poisoned the terminal state; it is an explicit failure, never a
    /// silent empty result.
    Poisoned,
    /// The terminal lock is held elsewhere: transient, not loss.
    Contended,
}

/// One nonblocking dequeue outcome. Contention is not loss and not an error.
enum QueuePoll {
    Block(PcmBlock),
    Empty,
    Contended,
}

/// Bounded native evidence recorded once during preparation and rendered by
/// [`WindowsMicrophoneSession::diagnostics`]. It is private, fixed size and never grows
/// per packet: the adopted device layout, the native buffer capacity and the clock anchor
/// evidence the anchors were built from.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NativeDiagnostics {
    sample_rate: NonZeroU32,
    channels: NonZeroU16,
    encoding: super::microphone::format::SampleEncoding,
    block_align: u32,
    native_capacity_frames: u32,
    evidence: native::ClockEvidence,
}

pub(crate) struct Shared {
    queue: Mutex<QueueState>,
    terminal: Mutex<Option<Terminal>>,
    diagnostics: Arc<SharedDiagnostics>,
    native: Mutex<Option<NativeDiagnostics>>,
    cancellation: AudioCancellation,
    stop: AtomicBool,
    /// A graceful drain request, deliberately separate from `stop` (abort).
    drain: AtomicBool,
    worker_finished: AtomicBool,
    /// Typed proof from the real buffer/Stop/resource-release path, never from
    /// an error code or diagnostic message. Published after apartment release.
    continuity_released: AtomicBool,
}

impl Shared {
    fn new(cancellation: AudioCancellation) -> Self {
        Self {
            queue: Mutex::new(QueueState::default()),
            terminal: Mutex::new(None),
            diagnostics: Arc::new(SharedDiagnostics::default()),
            native: Mutex::new(None),
            cancellation,
            stop: AtomicBool::new(false),
            drain: AtomicBool::new(false),
            worker_finished: AtomicBool::new(false),
            continuity_released: AtomicBool::new(false),
        }
    }

    /// Records a graceful drain request. Idempotent and nonblocking; it never
    /// cancels, discards or joins.
    fn request_drain(&self) {
        self.drain.store(true, Ordering::Release);
    }

    fn drain_requested(&self) -> bool {
        self.drain.load(Ordering::Acquire)
    }

    /// The worker stops taking new native packets for an abort or a graceful drain.
    /// The packet it is already holding is still published before this is observed.
    fn capture_should_end(&self) -> bool {
        self.cancellation.is_cancelled()
            || self.stop.load(Ordering::Acquire)
            || self.drain_requested()
    }

    /// Publishes the terminal outcome of a worker that ended its packet loop: a
    /// requested drain keeps every published block, any other exit is the abort
    /// cleanup that discards untrustworthy PCM.
    fn capture_ended(&self) {
        if self.drain_requested()
            && !self.stop.load(Ordering::Acquire)
            && !self.cancellation.is_cancelled()
        {
            self.drained();
        } else {
            self.succeeded();
        }
    }

    /// Records a completed graceful drain without discarding published PCM. A
    /// poisoned lock is still an explicit failure, not a silent drain.
    fn drained(&self) {
        let queue_poisoned = self.seal_queue();
        let (mut terminal, terminal_poisoned) = match self.terminal.lock() {
            Ok(terminal) => (terminal, false),
            Err(poison) => (poison.into_inner(), true),
        };
        if matches!(terminal.as_ref(), Some(Terminal::Failed(_))) {
            return;
        }
        if queue_poisoned || terminal_poisoned {
            drop(terminal);
            self.fail(internal(
                "The microphone queue or terminal state was poisoned during drain.".into(),
                "Start a new microphone session.",
            ));
            return;
        }
        *terminal = Some(Terminal::Drained);
    }

    /// Records that the worker will publish nothing further while keeping every
    /// already-published block. Returns whether the queue was poisoned.
    fn seal_queue(&self) -> bool {
        let (mut queue, poisoned) = match self.queue.lock() {
            Ok(queue) => (queue, false),
            Err(error) => (error.into_inner(), true),
        };
        queue.closed = true;
        if poisoned {
            self.continuity_released.store(false, Ordering::Release);
        }
        poisoned
    }

    /// Blocks still awaiting the consumer. A completed drain must not be claimed
    /// while any of them would be discarded by a later stop.
    fn queued_blocks(&self) -> usize {
        match self.queue.lock() {
            Ok(queue) => queue.blocks.stats().depth,
            Err(poison) => poison.into_inner().blocks.stats().depth,
        }
    }

    /// Control/owner-thread cleanup must reclaim even a poisoned queue. Poison
    /// is retained as a failure; it is never permission to leak queued PCM.
    fn close_queue(&self) -> bool {
        let (mut queue, poisoned) = match self.queue.lock() {
            Ok(queue) => (queue, false),
            Err(error) => (error.into_inner(), true),
        };
        let discarded = queue.discard();
        self.diagnostics
            .discarded_bytes
            .fetch_add(discarded, Ordering::AcqRel);
        if poisoned {
            self.continuity_released.store(false, Ordering::Release);
        }
        poisoned
    }

    fn fail(&self, error: RecorderError) {
        {
            let mut terminal = self
                .terminal
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if !matches!(terminal.as_ref(), Some(Terminal::Failed(_))) {
                *terminal = Some(Terminal::Failed(error));
            }
        }
        self.close_queue();
    }

    fn succeeded(&self) {
        let queued = self.queued_blocks();
        let queue_poisoned = self.close_queue();
        let (mut terminal, terminal_poisoned) = match self.terminal.lock() {
            Ok(terminal) => (terminal, false),
            Err(poison) => (poison.into_inner(), true),
        };
        if terminal_poisoned {
            self.continuity_released.store(false, Ordering::Release);
        }
        if matches!(terminal.as_ref(), Some(Terminal::Failed(_))) {
            return;
        }
        // A completed drain survives a later stop only when that stop discarded
        // nothing: an abort that dropped published PCM is not a completed drain.
        if matches!(terminal.as_ref(), Some(Terminal::Drained))
            && queued == 0
            && !queue_poisoned
            && !terminal_poisoned
        {
            return;
        }
        *terminal = Some(if queue_poisoned || terminal_poisoned {
            Terminal::Failed(internal(
                "The microphone queue or terminal state was poisoned during cleanup.".into(),
                "Start a new microphone session.",
            ))
        } else {
            Terminal::Stopped
        });
    }
}

/// One running Windows microphone session (FRAME-0105).
///
/// It exposes no COM object, handle, pointer or native buffer: only the canonical
/// [`frame_audio::PcmBlock`] stream, its bounded diagnostics and the accepted session
/// operations.
pub struct WindowsMicrophoneSession {
    shared: Arc<Shared>,
    worker: Option<std::thread::JoinHandle<()>>,
    source: AudioSourceId,
    stopped: bool,
    stop_result: Option<RecorderError>,
}

impl WindowsMicrophoneSession {
    /// Human-readable diagnostics for the read-only probe.
    ///
    /// Diagnostic text for a person to read, not a data API: it exposes no native type,
    /// handle or model. A poisoned diagnostics lock is reported, never silently hidden.
    pub fn diagnostics(&self) -> Result<String, RecorderError> {
        let mut text = String::new();
        let reference = Arc::clone(&self.shared.diagnostics);
        let native_record = match self.shared.native.lock() {
            Ok(native) => *native,
            Err(_) => {
                return Err(internal(
                    "The microphone native state was poisoned, so its evidence is not reportable."
                        .to_string(),
                    "Start a new microphone session.",
                ));
            }
        };
        text.push_str(&format!(
            "microphone: source={} released={} worker_finished={}\n",
            self.source.0,
            self.stopped,
            self.shared.worker_finished.load(Ordering::Acquire)
        ));
        match native_record {
            Some(native) => {
                text.push_str(&format!(
                    "format: sample_rate={} channels={} encoding={} block_align={} native_capacity_frames={} sample_format=interleaved f32\n",
                    native.sample_rate,
                    native.channels,
                    native.encoding.describe(),
                    native.block_align,
                    native.native_capacity_frames
                ));
                text.push_str(&format!(
                    "clock: raw_qpc_ticks={} qpc_frequency={} qpc_100ns={} bracket_ns={} uncertainty_ns={}\n",
                    native.evidence.raw_qpc_ticks,
                    native.evidence.qpc_frequency,
                    native.evidence.qpc_100ns,
                    native.evidence.bracket_ns,
                    native.evidence.uncertainty_ns
                ));
            }
            None => text.push_str(
                "format: not reported by the device yet; clock: no anchored evidence yet\n",
            ),
        }
        text.push_str(&format!(
            "packets: total={} empty={} silent={} frames={} pre_epoch_discarded={} discontinuity_flags={} queue_overflows={} discarded_bytes={} held_high_water={}\n",
            reference.packets.load(Ordering::Acquire),
            reference.empty_packets.load(Ordering::Acquire),
            reference.silent_packets.load(Ordering::Acquire),
            reference.total_frames.load(Ordering::Acquire),
            reference.pre_epoch_discarded.load(Ordering::Acquire),
            reference.discontinuity_flags.load(Ordering::Acquire),
            reference.queue_overflows.load(Ordering::Acquire),
            reference.discarded_bytes.load(Ordering::Acquire),
            reference.held_high_water.load(Ordering::Acquire),
        ));
        let queue = match self.shared.queue.lock() {
            Ok(queue) => queue,
            Err(_) => {
                return Err(internal(
                    "The microphone queue was poisoned, so its state is not reportable."
                        .to_string(),
                    "Start a new microphone session.",
                ));
            }
        };
        text.push_str(&format!(
            "queue: queued_blocks={} held_bytes={} closed={} limit_blocks={} limit_bytes={}\n",
            queue.blocks.stats().depth,
            queue.held_bytes,
            queue.closed,
            MAX_QUEUED_BLOCKS,
            MAX_OWNED_PCM_BYTES
        ));
        drop(queue);
        let observations = match reference.observations.lock() {
            Ok(observations) => observations.clone(),
            Err(_) => {
                return Err(internal(
                    "The microphone observation log was poisoned.".to_string(),
                    "Start a new microphone session.",
                ));
            }
        };
        text.push_str(&format!("observations: {}\n", observations.len()));
        for observation in observations.iter().take(8) {
            text.push_str(&format!(
                "  frames={} device_position={} raw_qpc_100ns={} mapped_ns={} pcm_timestamp_ns={} silent={} discontinuity={} timestamp_error={}\n",
                observation.frames,
                observation.device_position,
                observation.raw_qpc_100ns,
                observation.mapped_ns,
                observation.pcm_timestamp_ns,
                observation.silent,
                observation.discontinuity,
                observation.timestamp_error
            ));
        }
        Ok(text)
    }
}

impl WindowsMicrophoneSession {
    /// Samples the worker's completion marker without waiting: a finished handle is
    /// as good as the published flag.
    fn worker_finished(&self) -> bool {
        self.shared.worker_finished.load(Ordering::Acquire)
            || self
                .worker
                .as_ref()
                .is_some_and(|worker| worker.is_finished())
    }

    /// One nonblocking read of the worker's terminal outcome.
    fn poll_terminal(&self) -> TerminalPoll {
        match self.shared.terminal.try_lock() {
            Ok(terminal) => match terminal.as_ref() {
                Some(Terminal::Failed(error)) => TerminalPoll::Failed(error.clone()),
                Some(Terminal::Drained) => TerminalPoll::Drained,
                Some(Terminal::Stopped) => TerminalPoll::Ended,
                None => TerminalPoll::Unpublished,
            },
            Err(TryLockError::Poisoned(poisoned)) => match poisoned.into_inner().as_ref() {
                Some(Terminal::Failed(error)) => TerminalPoll::Failed(error.clone()),
                _ => TerminalPoll::Poisoned,
            },
            Err(TryLockError::WouldBlock) => TerminalPoll::Contended,
        }
    }

    /// The single dequeue path, shared by `try_pcm` and `poll_drain`, so alternating
    /// them can never return the same published block twice.
    fn take_block(&mut self) -> Result<QueuePoll, RecorderError> {
        let mut queue = match self.shared.queue.try_lock() {
            Ok(queue) => queue,
            Err(TryLockError::Poisoned(poisoned)) => {
                let mut queue = poisoned.into_inner();
                let discarded = queue.discard();
                self.shared
                    .diagnostics
                    .discarded_bytes
                    .fetch_add(discarded, Ordering::AcqRel);
                self.shared.cancellation.cancel();
                return Err(internal(
                    "The microphone queue was poisoned by a panic, so its buffered PCM is discarded."
                        .to_string(),
                    "The source is failed; start a new microphone session.",
                ));
            }
            // Consumer contention is not loss and not an error.
            Err(TryLockError::WouldBlock) => return Ok(QueuePoll::Contended),
        };
        match queue.blocks.pop() {
            Some(block) => {
                queue.held_bytes = queue.held_bytes.saturating_sub(charged_bytes(&block));
                Ok(QueuePoll::Block(block))
            }
            None => Ok(QueuePoll::Empty),
        }
    }

    /// The checks both poll methods share before any block is delivered. A sticky
    /// failure precedes buffered PCM; a lock held elsewhere is transient. `Ok(None)`
    /// means the caller must report "nothing yet" without touching the queue.
    fn poll_terminal_gate(
        &self,
        unavailable_outcome: &str,
    ) -> Result<Option<TerminalPoll>, RecorderError> {
        // Sample completion before reading the terminal result: if the worker finishes
        // between those reads, its published result must not look like a missing result.
        let worker_finished = self.worker_finished();
        let terminal = self.poll_terminal();
        match &terminal {
            TerminalPoll::Failed(error) => return Err(error.clone()),
            TerminalPoll::Poisoned => {
                return Err(internal(
                    "The microphone session state was poisoned, so its buffered PCM is no longer trustworthy."
                        .to_string(),
                    "The source is failed; start a new microphone session.",
                ));
            }
            // A lock held elsewhere means "unknown yet", not a stall and not loss.
            TerminalPoll::Contended => return Ok(None),
            _ => {}
        }
        // A worker that ended without publishing a result is a lifecycle failure,
        // never a quiet loop of empty polls.
        if matches!(terminal, TerminalPoll::Unpublished) && worker_finished && !self.stopped {
            return Err(internal(
                format!(
                    "The microphone worker ended without publishing a result, so this session cannot {unavailable_outcome}."
                ),
                "The source is failed; start a new microphone session.",
            ));
        }
        if let Some(error) = &self.stop_result {
            return Err(error.clone());
        }
        Ok(Some(terminal))
    }
}

impl AudioSession for WindowsMicrophoneSession {
    fn pcm_channels(&self) -> NonZeroU16 {
        self.shared
            .native
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map_or(NonZeroU16::new(2).unwrap(), |native| native.channels)
    }

    fn try_pcm(&mut self) -> Result<Option<PcmBlock>, RecorderError> {
        if self.poll_terminal_gate("deliver more PCM")?.is_none() {
            return Ok(None);
        }
        match self.take_block()? {
            QueuePoll::Block(block) => Ok(Some(block)),
            QueuePoll::Empty | QueuePoll::Contended => Ok(None),
        }
    }

    /// Idempotent: cancels the source, wakes and joins the worker, then releases every
    /// native resource on the thread that owns them.
    ///
    /// An explicit cleanup failure is never reported as a successful stop: the primary
    /// failure and the cleanup failure are both preserved.
    fn stop(&mut self) -> Result<(), RecorderError> {
        if self.stopped {
            return match &self.stop_result {
                Some(error) => Err(error.clone()),
                None => Ok(()),
            };
        }
        self.stopped = true;
        self.shared.cancellation.cancel();
        self.shared.stop.store(true, Ordering::Release);
        let mut cleanup_error = None;
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                self.shared
                    .continuity_released
                    .store(false, Ordering::Release);
                cleanup_error = Some(internal(
                    "The microphone worker panicked, so its native stream may not have released cleanly."
                        .to_string(),
                    "Report this run; the source is failed and no PCM from it is trustworthy.",
                ));
            }
            self.shared.worker_finished.store(true, Ordering::Release);
        }
        self.shared.succeeded();
        let primary = match self.shared.terminal.lock() {
            Ok(terminal) => match terminal.as_ref() {
                Some(Terminal::Failed(error)) => Some(error.clone()),
                _ => None,
            },
            Err(poisoned) => {
                self.shared
                    .continuity_released
                    .store(false, Ordering::Release);
                match poisoned.into_inner().as_ref() {
                    Some(Terminal::Failed(error)) => Some(error.clone()),
                    _ => Some(internal(
                        "The microphone session state was poisoned during stop.".to_string(),
                        "Start a new microphone session.",
                    )),
                }
            }
        };
        let result = match (primary, cleanup_error) {
            (None, None) => Ok(()),
            (Some(primary), None) => Err(primary),
            (None, Some(cleanup)) => Err(cleanup),
            (Some(primary), Some(cleanup)) => Err(native::combine(primary, cleanup)),
        };
        if let Err(error) = &result {
            self.stop_result = Some(error.clone());
        }
        result
    }

    fn stop_for_continuity(&mut self) -> Result<(), RecorderError> {
        let result = self.stop();
        if self.worker.is_some()
            || !self.shared.worker_finished.load(Ordering::Acquire)
            || !self.shared.continuity_released.load(Ordering::Acquire)
        {
            let error = match result {
                Err(error) => error,
                Ok(()) => internal(
                    "Microphone native release was not certified for a continuity transition."
                        .into(),
                    "Stop this recording; do not open a replacement native stream.",
                ),
            };
            return Err(error);
        }
        match result {
            Err(error) if error.code == ErrorCode::SourceLost => Ok(()),
            result => result,
        }
    }
}

impl DrainableAudioSession for WindowsMicrophoneSession {
    /// Idempotent and nonblocking: it only records the request and never cancels,
    /// discards, joins or waits. Cancellation, join and discard stay in `stop`.
    fn request_drain(&mut self) -> Result<(), RecorderError> {
        if let Some(TerminalPoll::Ended) = self.poll_terminal_gate("begin a drain")? {
            return Err(aborted_drain());
        }
        self.shared.request_drain();
        Ok(())
    }

    /// At most one already-published block per call. `Pending` covers a lock held
    /// elsewhere and an unfinished worker; `Drained` requires a successfully
    /// finished drain *and* an empty queue, so it can never be claimed while a
    /// block is still queued or hidden behind a contended lock. An aborted session
    /// reports an explicit transition error instead of waiting forever.
    fn poll_drain(&mut self) -> Result<PcmDrainPoll, RecorderError> {
        let Some(terminal) = self.poll_terminal_gate("confirm a drain")? else {
            return Ok(PcmDrainPoll::Pending);
        };
        if matches!(terminal, TerminalPoll::Ended) {
            return Err(aborted_drain());
        }
        match self.take_block()? {
            QueuePoll::Block(block) => Ok(PcmDrainPoll::Block(block)),
            // Contention can still hide a published block, so it is never a verdict.
            QueuePoll::Contended => Ok(PcmDrainPoll::Pending),
            QueuePoll::Empty => {
                if matches!(terminal, TerminalPoll::Drained) {
                    Ok(PcmDrainPoll::Drained)
                } else {
                    Ok(PcmDrainPoll::Pending)
                }
            }
        }
    }
}

fn aborted_drain() -> RecorderError {
    RecorderError {
        code: ErrorCode::InvalidTransition,
        message: "This audio session was aborted; a graceful drain cannot complete.".into(),
        recovery: RecoveryStatus::NoRecording,
        action: "Start a new audio session.".into(),
    }
}

impl Drop for WindowsMicrophoneSession {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// The worker's single settlement point: publish the terminal outcome, then mark the
/// worker finished. A requested drain keeps every published block; any other exit is
/// the existing abort cleanup that discards untrustworthy PCM.
fn settle_worker(shared: &Arc<Shared>, outcome: Result<(), RecorderError>) {
    match outcome {
        Ok(()) => shared.capture_ended(),
        Err(error) => shared.fail(error),
    }
    shared.worker_finished.store(true, Ordering::Release);
}

/// Starts real capture of exactly `source` on a fresh joined MTA worker.
pub(crate) fn start_microphone(
    source: &AudioSourceId,
    context: AudioContext,
) -> Result<WindowsMicrophoneSession, RecorderError> {
    let endpoint = endpoint_of(source)
        .ok_or_else(|| {
            RecorderError::invalid(format!(
                "{} is not a Windows microphone source identity.",
                source.0
            ))
        })?
        .to_string();
    start_prepared(source, context, move |checkpoint| {
        native::prepare_capture(&endpoint, checkpoint)
    })
}

/// Shared owner/queue/clock lifecycle; only native preparation differs by source.
pub(crate) fn start_prepared<F>(
    source: &AudioSourceId,
    context: AudioContext,
    prepare: F,
) -> Result<WindowsMicrophoneSession, RecorderError>
where
    F: FnOnce(
            &dyn Fn() -> Result<(), RecorderError>,
        ) -> Result<native::PreparedCapture, RecorderError>
        + Send
        + 'static,
{
    if context.cancellation.is_cancelled() {
        return Err(cancelled_start());
    }
    let worker_source = source.0.clone();
    let shared = Arc::new(Shared::new(context.cancellation.clone()));
    let clock = Arc::clone(&context.clock);
    let worker_shared = Arc::clone(&shared);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<(), RecorderError>>(1);

    let worker = std::thread::Builder::new()
        .name("frame-microphone".to_string())
        .spawn(move || {
            // A worker panic becomes a sticky terminal failure instead of a silent stall.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                capture_worker(&worker_source, &clock, &worker_shared, &ready_tx, prepare)
            }));
            let outcome = match outcome {
                Ok(result) => result,
                Err(_) => Err(internal(
                    "The microphone worker panicked, so this source is failed.".to_string(),
                    "Start a new microphone session.",
                )),
            };
            settle_worker(&worker_shared, outcome);
        })
        .map_err(|error| {
            internal(
                format!("The microphone worker thread could not be created: {error}"),
                "Retry the microphone start.",
            )
        })?;

    // Readiness is awaited in bounded slices so cancellation wins before Start.
    let deadline = Instant::now() + PREPARATION_WATCHDOG;
    loop {
        if context.cancellation.is_cancelled() {
            shared.cancellation.cancel();
            shared.stop.store(true, Ordering::Release);
            return Err(join_preparation_failure(worker, &shared, cancelled_start()));
        }
        match ready_rx.recv_timeout(Duration::from_millis(20)) {
            Ok(Ok(())) => break,
            Ok(Err(error)) => {
                return Err(join_preparation_failure(worker, &shared, error));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() >= deadline {
                    shared.cancellation.cancel();
                    shared.stop.store(true, Ordering::Release);
                    return Err(join_preparation_failure(
                        worker,
                        &shared,
                        internal(
                            format!(
                                "Microphone preparation did not finish within {} ms.",
                                PREPARATION_WATCHDOG.as_millis()
                            ),
                            "The worker was cancelled and joined; a native call that cannot be interrupted is not cancellable.",
                        ),
                    ));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(join_preparation_failure(
                    worker,
                    &shared,
                    internal(
                        "The microphone worker disconnected before reporting readiness."
                            .to_string(),
                        "The source failed during preparation; start a new session.",
                    ),
                ));
            }
        }
    }
    Ok(WindowsMicrophoneSession {
        shared,
        worker: Some(worker),
        source: source.clone(),
        stopped: false,
        stop_result: None,
    })
}

/// The control thread must retain failures discovered while joining cancellation
/// or timeout cleanup, not replace them with a generic preparation error.
fn join_preparation_failure(
    worker: std::thread::JoinHandle<()>,
    shared: &Shared,
    primary: RecorderError,
) -> RecorderError {
    if worker.join().is_err() {
        return native::combine(primary, lifecycle::panic_error());
    }
    let terminal = shared
        .terminal
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if let Some(Terminal::Failed(error)) = terminal.as_ref()
        && error.message != primary.message
    {
        return native::combine(primary, error.clone());
    }
    primary
}

/// The owner worker: prepare, publish readiness, stream, and release on every exit.
fn capture_worker(
    endpoint: &str,
    clock: &Arc<MasterClock>,
    shared: &Arc<Shared>,
    ready: &std::sync::mpsc::SyncSender<Result<(), RecorderError>>,
    prepare: impl FnOnce(
        &dyn Fn() -> Result<(), RecorderError>,
    ) -> Result<native::PreparedCapture, RecorderError>,
) -> Result<(), RecorderError> {
    let deadline = Instant::now() + PREPARATION_WATCHDOG;
    let checkpoint = || preparation_checkpoint(shared, deadline);
    let mut announced = false;
    let mut continuity_released = false;
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        native::with_apartment(|| {
            checkpoint()?;
            let prepared = prepare(&checkpoint)?;
            let format = prepared.format();
            checkpoint()?;
            let frequency = native::qpc_frequency().ok_or_else(|| {
                internal(
                    "The OS reported no performance counter frequency.".into(),
                    "This machine cannot provide the accepted clock anchor.",
                )
            })?;
            checkpoint()?;
            let (anchor, evidence) = native::qpc_anchor(clock, frequency).ok_or_else(|| {
                internal(
                    "No usable QPC bracket within 1 ms was obtained.".into(),
                    "This machine cannot provide the accepted clock anchor.",
                )
            })?;
            checkpoint()?;
            *shared.native.lock().map_err(|_| {
                internal(
                    "The microphone native diagnostics were poisoned during preparation.".into(),
                    "Start a new microphone session.",
                )
            })? = Some(NativeDiagnostics {
                sample_rate: format.sample_rate(),
                channels: format.channels(),
                encoding: format.encoding(),
                block_align: format.block_align(),
                native_capacity_frames: format.buffer_frames(),
                evidence,
            });
            let capture = prepared.start(checkpoint)?;
            let (result, released) = run_started_with_release(capture, |capture| {
                announce_ready(shared, deadline, ready)?;
                announced = true;
                stream_packets(capture, endpoint, shared, format, anchor)
            });
            continuity_released = released;
            result
        })
    }));
    let no_unwind = outcome.is_ok();
    let outcome = outcome.unwrap_or_else(|_| Err(lifecycle::panic_error()));
    shared
        .continuity_released
        .store(continuity_released && no_unwind, Ordering::Release);
    // No preparation error is sent before native owners/apartment are released.
    // Never enqueue a second message behind successful readiness.
    if !announced && let Err(error) = &outcome {
        let _ = ready.try_send(Err(error.clone()));
    }
    outcome
}

fn preparation_checkpoint(shared: &Shared, deadline: Instant) -> Result<(), RecorderError> {
    if shared.cancellation.is_cancelled() || shared.stop.load(Ordering::Acquire) {
        return Err(cancelled_start());
    }
    if Instant::now() >= deadline {
        return Err(internal(
            "Microphone preparation exceeded its five-second watchdog.".into(),
            "The worker checks between native calls; an in-progress Windows call cannot be interrupted.",
        ));
    }
    Ok(())
}

fn announce_ready(
    shared: &Shared,
    deadline: Instant,
    ready: &std::sync::mpsc::SyncSender<Result<(), RecorderError>>,
) -> Result<(), RecorderError> {
    preparation_checkpoint(shared, deadline)?;
    ready.try_send(Ok(())).map_err(|_| {
        internal(
            "The microphone readiness receiver is unavailable.".into(),
            "Start a new microphone session.",
        )
    })
}

fn stream_packets(
    capture: &impl CaptureStream,
    endpoint: &str,
    shared: &Arc<Shared>,
    format: DeviceFormat,
    anchor: ClockAnchor,
) -> Result<(), RecorderError> {
    let mut published = false;
    let mut last_first_sample: Option<u64> = None;
    let mut last_end_position: Option<u64> = None;
    let mut device_anchor: Option<(u64, MediaTimestamp)> = None;
    let mut last_frames = 0u32;
    let mut drained_in_batch = 0u32;
    let mut scratch: Vec<f32> = Vec::new();
    let sample_rate = format.sample_rate();
    let channels = format.channels();

    loop {
        if shared.capture_should_end() {
            return Ok(());
        }
        // Bounded drain accounting: a packet that is already available is consumed
        // without ever waiting for a future event (see `next_wait_ms`), cancellation is
        // re-checked before every attempt at the top of this loop, and after a full
        // batch of MAX_DRAIN_BATCH attempts control state is re-checked and the worker
        // yields instead of imposing an event wait on pending packets.
        drained_in_batch += 1;
        if drained_in_batch >= native::MAX_DRAIN_BATCH {
            drained_in_batch = 0;
            if shared.capture_should_end() {
                return Ok(());
            }
            std::thread::yield_now();
        }
        let packet = capture.next_packet(&mut scratch, WAIT_SLICE_MS)?;
        let Some(observation) = packet else {
            shared
                .diagnostics
                .empty_packets
                .fetch_add(1, Ordering::AcqRel);
            continue;
        };
        shared.diagnostics.packets.fetch_add(1, Ordering::AcqRel);
        if observation.silent {
            shared
                .diagnostics
                .silent_packets
                .fetch_add(1, Ordering::AcqRel);
        }
        if observation.discontinuity {
            shared
                .diagnostics
                .discontinuity_flags
                .fetch_add(1, Ordering::AcqRel);
        }
        if observation.timestamp_error {
            return Err(source_lost(
                "The device flagged this packet's timestamp as invalid, so its first-sample time is unknown."
                    .to_string(),
            ));
        }
        if observation.frames == 0 {
            shared
                .diagnostics
                .empty_packets
                .fetch_add(1, Ordering::AcqRel);
            continue;
        }
        // ADR 0007: the QPC position is already in 100 ns units and is mapped through
        // the accepted bracketed anchor; nothing is timestamped at dequeue time.
        let mapped = match anchor.map(observation.qpc_100ns) {
            Some(mapped) => mapped,
            None => {
                if !published {
                    shared
                        .diagnostics
                        .pre_epoch_discarded
                        .fetch_add(1, Ordering::AcqRel);
                    continue;
                }
                return Err(source_lost(
                    "The device reported a first-sample time before the recording epoch after capture had started."
                        .to_string(),
                ));
            }
        };
        if published {
            if observation.discontinuity && !capture.allows_packet_gaps() {
                return Err(source_lost(
                    "The device reported a data discontinuity after capture had started, so this timeline is no longer continuous."
                        .to_string(),
                ));
            }
            if let Some(previous) = last_first_sample
                && mapped.0 <= previous
            {
                return Err(source_lost(format!(
                    "The device reported a non-increasing first-sample time ({} ns after {previous} ns).",
                    mapped.0
                )));
            }
            // Continuity is exact: this packet must start where the previous one
            // ended. A forward gap and an overlap are both failures.
            if capture.device_position_available()
                && let Some(expected_start) = last_end_position
                && observation.device_position != expected_start
                && !capture.allows_packet_gaps()
            {
                return Err(source_lost(format!(
                    "The device reported the frame position {} where {} was expected after the previous packet.",
                    observation.device_position, expected_start
                )));
            }
        }
        if !capture.device_position_available()
            && let Some(previous) = last_first_sample
        {
            crate::application_audio::validate_qpc_interval(
                previous,
                last_frames,
                mapped.0,
                sample_rate.get(),
            )?;
        }
        let Some(end_position) = observation
            .device_position
            .checked_add(u64::from(observation.frames))
        else {
            return Err(source_lost(
                "The device frame position overflowed.".to_string(),
            ));
        };
        // Endpoint packet QPC observations can jitter even while device frames are
        // contiguous. Preserve the first measured epoch; advance on the device's
        // sample grid without accumulating rounding. Raw QPC remains diagnostic.
        // Process loopback has no device position and keeps its separate QPC policy.
        if capture.allows_packet_gaps()
            && (observation.discontinuity
                || last_end_position.is_some_and(|p| p != observation.device_position))
        {
            device_anchor = None;
        }
        let timestamp = if capture.device_position_available() {
            let (first_position, first_time) =
                *device_anchor.get_or_insert((observation.device_position, mapped));
            observation
                .device_position
                .checked_sub(first_position)
                .and_then(|frames| sample_timestamp(frames, sample_rate))
                .and_then(|elapsed| first_time.0.checked_add(elapsed.0))
                .map(MediaTimestamp)
                .ok_or_else(|| source_lost("The device sample timeline overflowed.".into()))?
        } else {
            mapped
        };
        last_frames = observation.frames;
        last_first_sample = Some(mapped.0);
        last_end_position = Some(end_position);
        shared
            .diagnostics
            .total_frames
            .fetch_add(u64::from(observation.frames), Ordering::AcqRel);
        shared.diagnostics.observe(Observation {
            frames: observation.frames,
            device_position: observation.device_position,
            raw_qpc_100ns: observation.qpc_100ns,
            mapped_ns: mapped.0,
            pcm_timestamp_ns: timestamp.0,
            silent: observation.silent,
            discontinuity: observation.discontinuity,
            timestamp_error: observation.timestamp_error,
        })?;

        let block = PcmBlock {
            source_id: AudioSourceId(endpoint.to_owned()),
            timestamp,
            sample_rate,
            channels,
            samples: std::mem::take(&mut scratch),
        };
        published = true;
        publish(shared, block)?;
    }
}

/// Publishes one decoded block into the bounded queue.
///
/// Preserve the already-budgeted packet across brief consumer contention. The
/// consumer stays nonblocking; poison and a 10 ms contention deadline fail visibly.
fn queue_for_publish(
    shared: &Shared,
    mut wait: impl FnMut(),
) -> Result<MutexGuard<'_, QueueState>, RecorderError> {
    let deadline = Instant::now() + Duration::from_millis(10);
    loop {
        match shared.queue.try_lock() {
            Ok(queue) => return Ok(queue),
            Err(TryLockError::Poisoned(_)) => {
                return Err(internal(
                    "The audio queue was poisoned.".into(),
                    "Start a new audio session.",
                ));
            }
            Err(TryLockError::WouldBlock) => {}
        }
        if Instant::now() >= deadline {
            return Err(source_lost("The audio queue lock remained contended for 10 ms; this source cannot preserve continuity.".into()));
        }
        wait();
    }
}

/// A full queue or prolonged contention terminates this source explicitly:
/// audio is never silently dropped and continuity is never claimed.
fn publish(shared: &Arc<Shared>, block: PcmBlock) -> Result<(), RecorderError> {
    let bytes = charged_bytes(&block);
    if bytes > MAX_PCM_BLOCK_BYTES {
        return Err(internal(
            "The microphone PCM allocation exceeds its per-block budget.".into(),
            "Start a new microphone session.",
        ));
    }
    let mut queue = queue_for_publish(shared, || std::thread::sleep(Duration::from_millis(1)))?;
    if let Some(rejected) = queue.blocks.push(block) {
        shared
            .diagnostics
            .queue_overflows
            .fetch_add(1, Ordering::AcqRel);
        shared
            .diagnostics
            .discarded_bytes
            .fetch_add(charged_bytes(&rejected), Ordering::AcqRel);
        return Err(RecorderError {
            code: ErrorCode::SourceLost,
            message: format!(
                "The microphone's bounded queue is full ({} blocks, {} bytes held), so audio would have to be dropped.",
                queue.blocks.stats().depth,
                queue.held_bytes
            ),
            recovery: RecoveryStatus::NoRecording,
            action: "Consume microphone PCM faster or shorten the session; this source is failed rather than silently discontinuous."
                .into(),
        });
    }
    queue.held_bytes = queue.held_bytes.saturating_add(bytes);
    let held = queue.held_bytes;
    shared
        .diagnostics
        .held_high_water
        .fetch_max(held, Ordering::AcqRel);
    Ok(())
}

/// Enumerates the real active capture endpoints as canonical sources.
///
/// Names come from the device property store and duplicate names are valid; identity is
/// the full opaque endpoint ID, never a name or an ordinal.
pub(crate) fn enumerate_microphones() -> Result<Vec<AudioSource>, RecorderError> {
    let endpoints = native::enumerate_endpoints()?;
    let mut sources: Vec<AudioSource> = Vec::with_capacity(endpoints.len());
    for endpoint in endpoints {
        sources.push(AudioSource {
            controls: Default::default(),
            capture_identity: None,
            muted: false,
            id: source_id(&endpoint.endpoint_id),
            kind: AudioSourceKind::Microphone,
            display_name: endpoint
                .friendly_name
                .unwrap_or_else(|| "Unnamed microphone".to_string()),
            // Microphones have no process or application identity.
            process_id: None,
            app_identifier: None,
            // An active capture endpoint is available; that is not a permission grant.
            is_active: true,
            // Ignore by default: routing is a coordinator decision.
            route: AudioRoute::Ignore,
        });
    }
    sources.sort_by(|left, right| left.id.cmp(&right.id));
    for pair in sources.windows(2) {
        if pair[0].id == pair[1].id {
            return Err(internal(
                format!(
                    "Two active capture endpoints reported the same identity ({}).",
                    pair[0].id.0
                ),
                "This is an endpoint identity defect; report it instead of selecting one arbitrarily.",
            ));
        }
    }
    Ok(sources)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MicrophoneFormatCapability {
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub sample_encoding: String,
}
/// Reports the exact endpoint's validated WASAPI mix format without capturing audio.
/// This is format evidence, not a permission grant or a successful stream activation.
pub fn probe_microphone_format(
    source: &AudioSourceId,
) -> Result<MicrophoneFormatCapability, RecorderError> {
    let endpoint = endpoint_of(source)
        .filter(|id| !id.is_empty() && !id.contains('\0'))
        .ok_or_else(|| RecorderError::invalid("Choose a valid WASAPI microphone identity."))?
        .to_owned();
    std::thread::Builder::new()
        .name("freym-mic-format".into())
        .spawn(move || {
            let format = native::probe_endpoint_format(&endpoint)?;
            Ok(MicrophoneFormatCapability {
                sample_rate_hz: format.sample_rate().get(),
                channels: format.channels().get(),
                sample_encoding: format.encoding().describe().into(),
            })
        })
        .map_err(|e| {
            internal(
                format!("Could not create microphone format worker: {e}"),
                "Retry format discovery.",
            )
        })?
        .join()
        .map_err(|_| {
            internal(
                "Microphone format worker panicked.".into(),
                "Retry format discovery.",
            )
        })?
}

/// Human-readable enumeration diagnostics for the read-only probe.
pub fn microphone_diagnostics() -> Result<String, RecorderError> {
    let endpoints = native::enumerate_endpoints()?;
    let mut text = format!("microphone_endpoints: {}\n", endpoints.len());
    for endpoint in &endpoints {
        text.push_str(&format!(
            "  id={}{} name={:?}\n",
            MICROPHONE_PREFIX, endpoint.endpoint_id, endpoint.friendly_name
        ));
    }
    if endpoints.is_empty() {
        text.push_str(
            "  (no active capture endpoints were reported; this is an honest empty result, not a fallback)\n",
        );
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue_shared() -> Arc<Shared> {
        Arc::new(Shared::new(AudioCancellation::default()))
    }

    fn block(samples: usize, timestamp: u64) -> PcmBlock {
        PcmBlock {
            source_id: AudioSourceId("wasapi-mic:test".to_string()),
            timestamp: frame_clock::MediaTimestamp(timestamp),
            sample_rate: NonZeroU32::new(48_000).unwrap(),
            channels: NonZeroU16::new(2).unwrap(),
            samples: vec![0.0; samples],
        }
    }

    #[test]
    fn brief_queue_contention_recovers_without_losing_owned_pcm() {
        let shared = queue_shared();
        let mut consumer = Some(shared.queue.lock().unwrap());
        let mut waits = 0;
        let mut queue = queue_for_publish(&shared, || {
            waits += 1;
            drop(consumer.take());
        })
        .unwrap();
        assert_eq!(waits, 1);
        assert!(queue.blocks.push(block(4, 7)).is_none());
        assert_eq!(queue.blocks.pop().unwrap().timestamp.0, 7);
    }

    #[test]
    fn prolonged_queue_contention_remains_an_explicit_failure() {
        let shared = queue_shared();
        let _consumer = shared.queue.lock().unwrap();
        let error = queue_for_publish(&shared, || std::thread::sleep(Duration::from_millis(1)))
            .err()
            .unwrap();
        assert_eq!(error.code, ErrorCode::SourceLost);
        assert!(error.message.contains("10 ms"));
    }

    #[test]
    fn identities_are_the_opaque_endpoint_id_and_round_trip() {
        let id = source_id("{0.0.1.00000000}.{abc}");
        assert_eq!(id.0, "wasapi-mic:{0.0.1.00000000}.{abc}");
        assert_eq!(endpoint_of(&id), Some("{0.0.1.00000000}.{abc}"));
        assert_eq!(endpoint_of(&AudioSourceId("other".to_string())), None);
    }

    #[test]
    fn the_queue_bound_is_enforced_and_overflow_is_a_sticky_source_failure() {
        let shared = queue_shared();
        for index in 0..MAX_QUEUED_BLOCKS {
            publish(&shared, block(8, index as u64)).expect("within the bound");
        }
        let error = publish(&shared, block(8, 99)).expect_err("the bound must hold");
        assert_eq!(error.code, ErrorCode::SourceLost);
        assert_eq!(
            shared.diagnostics.queue_overflows.load(Ordering::Acquire),
            1
        );
        let held = shared.queue.lock().unwrap().held_bytes;
        assert_eq!(
            held,
            (MAX_QUEUED_BLOCKS as u64) * 8 * size_of::<f32>() as u64
        );
    }

    #[test]
    fn a_failure_precedes_buffered_pcm_and_closes_the_queue() {
        let shared = queue_shared();
        publish(&shared, block(4, 1)).unwrap();
        shared.fail(source_lost("test failure".to_string()));
        let queue = shared.queue.lock().unwrap();
        assert!(queue.closed);
        assert!(
            queue.blocks.stats().depth == 0,
            "untrustworthy PCM is discarded"
        );
        assert_eq!(queue.held_bytes, 0);
    }

    #[test]
    fn timestamps_map_through_the_anchor_and_never_clamp_backwards() {
        let rate = std::num::NonZeroU64::new(10_000_000).unwrap();
        let anchor = ClockAnchor::from_sample(
            1_000,
            rate,
            frame_clock::MediaTimestamp(2_000_000_000),
            frame_clock::MediaTimestamp(2_000_000_100),
        )
        .unwrap();
        assert_eq!(anchor.map(1_000).unwrap().0, 2_000_000_050);
        let near_epoch = ClockAnchor::from_sample(
            10_000,
            rate,
            frame_clock::MediaTimestamp(100),
            frame_clock::MediaTimestamp(100),
        )
        .unwrap();
        assert!(near_epoch.map(0).is_none(), "a pre-epoch time is refused");
        let _ = MasterClock::default();
    }

    #[test]
    fn a_silent_packet_never_reads_a_data_pointer() {
        let format = DeviceFormat::from_parts(
            format::FormatDescription {
                sample_rate: 48_000,
                channels: 1,
                bits_per_sample: 32,
                block_align: 4,
                valid_bits: 32,
                is_float: Some(true),
                extensible: false,
            },
            480,
        )
        .unwrap();
        let mut out = vec![7.0; 3];
        format.convert_into(None, 2, &mut out).unwrap();
        assert_eq!(out, vec![0.0, 0.0]);
    }
}
