//! One live screen + microphone + application-audio recording (FRAME-0201).
//!
//! [`AvRecording`] is the single control owner of a complete live A/V session. It binds
//! the already accepted native and shared pieces to one another and invents none of
//! them:
//!
//! - one DXGI display capture session ([`WindowsCaptureSession`]) for one explicit
//!   [`DisplayId`] at [`FpsMode::AutoMatchSource`], so every surface carries the media
//!   timestamp of the session's own clock;
//! - the hardware encoder opened on the capture device's own pool and geometry,
//!   with full display acquisition activated after downstream preparation;
//! - one [`AudioPipeline`] over the caller's explicit routes, wrapped by
//!   [`EncodedAudioPipeline`] so the accepted mixer feeds one Opus encoder per frozen
//!   [`TrackPlan`] entry;
//! - one [`MkvAvWriter`] in **live** mode, created from the video encoder's own stream
//!   description, the plan's own real Opus pre-skip values and the shared origin.
//!
//! Capture, encoding and audio retain their existing workers. Periodic file sync
//! and manifest publication use a bounded durability worker without media buffers.
//! One control thread drives this owner through
//! [`AvRecording::start`], a bounded [`AvRecording::poll`],
//! [`AvRecording::request_stop`] and [`AvRecording::finish`].
//!
//! One clock, one origin
//! ---------------------
//!
//! The caller supplies exactly one `Arc<MasterClock>` and the one [`MediaTimestamp`] it
//! read from that clock before the session started. The capture session and every audio
//! source receive that same clock, and every packet keeps its real position on the shared
//! timeline: the container subtracts the origin, and nothing here rebases, shifts,
//! resamples or re-times a packet.
//!
//! Interleave, not a reordering queue
//! ----------------------------------
//!
//! Packets from the two encoders pass through one bounded interleave buffer
//! ([`MAX_INTERLEAVE_PACKETS`], [`MAX_INTERLEAVE_BYTES`]). It drains available packets
//! in (PTS, track) order after every announced track delivered its first packet and
//! the oldest packet is [`DELIVERY_DELAY`] behind the furthest observed timestamp.
//! A later delivery from another track may have an earlier PTS; the writer preserves
//! that PTS on disk. A packet behind its own track's written PTS, or a buffer that
//! exceeds either budget, is a terminal error. Nothing is dropped or re-timed.
//!
//! Stop
//! ----
//!
//! [`AvRecording::request_stop`] closes admission immediately and permanently: no capture
//! surface is read and no live audio quantum is encoded after it. The drain then runs in
//! the order the accepted pieces require — the video encoder's owned input and end of
//! stream, the audio drain and its real Opus trim, the remaining ordered packets, worker
//! join, container finalize. A failure is sticky and releases every resource without an
//! artifact. A declared track whose completed drain accepted zero PCM is a terminal
//! failure: the container requires every announced track to contain recorded audio.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use frame_audio::metering::MeterHub;
use frame_audio::{AudioPipeline, NormalizingBackend, RecoveringAudioSession, TrackPlan};
use frame_capture::{CaptureCancellation, CaptureContext, CaptureSession, CapturedFrame};
use frame_clock::{MasterClock, MediaTimestamp, PauseTimeline};
use frame_contracts::{
    AudioRoute, AudioRouteSelection, CaptureTarget, DisplayId, ErrorCode, FpsMode, RecorderError,
    RecordingArtifact, RecoveryStatus,
};
use frame_encode::admission::{FrameAdmission, FrameAdmitter};
use frame_encode::audio_pipeline::{EncodedAudioDrainPoll, EncodedAudioPipeline};
use frame_encode::opus::OpusTrackFinish;
use frame_encode::{
    EncodedPacket, EncoderInput, EncoderPoll, PacketEncoder, SubmitOutcome, nominal_duration,
};
use frame_mux::{AvAudioTrack, MkvAvWriter};
use frame_windows::{
    NativeEncoderConfig, NativeEncoderSettings, PlatformBackend, WindowsAudioSession,
    WindowsH264Encoder, WindowsSurface,
};

type NormalizedAudio = RecoveringAudioSession<WindowsAudioSession>;
type Capture = Box<dyn CaptureSession<Surface = WindowsSurface>>;

/// Guards each source admitted by AudioPipeline without changing its shared backend.
struct PreparationAudioBackend<'a, B> {
    backend: B,
    cancel: &'a AtomicBool,
}

impl<B: frame_audio::AudioBackend> frame_audio::AudioBackend for PreparationAudioBackend<'_, B> {
    fn enumerate_audio_sources(&self) -> Result<Vec<frame_contracts::AudioSource>, RecorderError> {
        crate::recording_control::check_preparation_cancelled(self.cancel)?;
        self.backend.enumerate_audio_sources()
    }
}

impl<B: frame_audio::AudioCaptureBackend> frame_audio::AudioCaptureBackend
    for PreparationAudioBackend<'_, B>
{
    type Session = B::Session;

    fn start_audio(
        &mut self,
        source: &frame_contracts::AudioSourceId,
        context: frame_audio::AudioContext,
    ) -> Result<Self::Session, RecorderError> {
        use frame_audio::AudioSession as _;
        crate::recording_control::check_preparation_cancelled(self.cancel)?;
        let mut session = self.backend.start_audio(source, context)?;
        if let Err(error) = crate::recording_control::check_preparation_cancelled(self.cancel) {
            return Err(combine_errors(Some(error), session.stop().err()).expect("cancellation"));
        }
        Ok(session)
    }
}

/// The bounded wall-clock budget for the first completed capture surface.
///
/// Preparation is the one place this owner waits for native work: a display that never
/// delivers a completed surface is reported instead of waited out.
pub const PREPARATION_WATCHDOG: Duration = Duration::from_secs(5);

/// The bounded wall-clock budget between two observable steps while this recording still
/// owns work (a pending input, an accepted-but-unfinished input, an outstanding end of
/// stream, an unflushed interleave buffer or an unfinished audio drain).
///
/// A static desktop that simply delivers no new surface owns no work and is never failed
/// for staying idle. A native call inside a worker cannot be forcibly cancelled, so a real
/// stall is reported instead of waited out.
pub const PROGRESS_WATCHDOG: Duration = Duration::from_secs(5);

/// How long a packet waits for a possible earlier one before it is written.
///
/// This is the delivery delay of the live interleave: a packet is written once the
/// furthest observed timestamp moved at least this far past it.
pub const DELIVERY_DELAY: Duration = Duration::from_millis(500);

/// At most this many packets wait for delivery at one time.
pub const MAX_INTERLEAVE_PACKETS: usize = 2048;

/// At most this many payload bytes wait for delivery at one time.
pub const MAX_INTERLEAVE_BYTES: u64 = 64 * 1024 * 1024;

/// How many packets one `poll` may write to the container.
///
/// The drain writes bounded batches, so a poll can never loop for an unbounded time even
/// while both encoders keep producing; the following polls write the rest.
const POLL_PACKET_BUDGET: usize = 512;

/// How many live audio quanta one `poll` may encode, so audio keeps pace with the caller's
/// cadence without unbounded work inside one step.
const POLL_AUDIO_BUDGET: usize = 16;

/// How many PCM blocks one `poll` may take off the native source queues. Every block is
/// paired with a mixer quantum, and the mixer's own accepted pending capacity is what
/// bounds how far ahead of the mixer this may run.
const POLL_AUDIO_SOURCE_BUDGET: usize = 32;

/// How many drain steps one `poll` may take from the audio pipeline.
const POLL_AUDIO_DRAIN_BUDGET: usize = 64;

/// How long preparation waits between two readiness checks.
const PREPARATION_POLL: Duration = Duration::from_millis(2);

// --------------------------------------------------------------------- errors

fn invalid_request_error(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::InvalidRequest,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: "Review the recording request and try again.".to_string(),
    }
}

fn preparation_error(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::SourceLost,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: "Verify the selected display and audio sources are active and try again."
            .to_string(),
    }
}

fn invalid_transition_error(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::InvalidTransition,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: "Follow the recording lifecycle: start, poll, request_stop, poll until \
                 drained, then finish."
            .to_string(),
    }
}

fn internal_error(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message,
        recovery: RecoveryStatus::Unknown,
        action: "The recording is incomplete: discard the artifact and report the failure."
            .to_string(),
    }
}

fn budget_error(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message,
        recovery: RecoveryStatus::Unknown,
        action: "The interleave budget is exhausted, so a packet would have to be dropped or \
                 re-timed; the recording stopped instead."
            .to_string(),
    }
}

fn stalled_error(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message,
        recovery: RecoveryStatus::Unknown,
        action: "The stream is incomplete: discard the artifact and report the observed stall."
            .to_string(),
    }
}

/// Keeps the first failure and appends teardown context, so neither is lost.
fn combine_errors(
    first: Option<RecorderError>,
    second: Option<RecorderError>,
) -> Option<RecorderError> {
    match (first, second) {
        (Some(first), Some(second)) => Some(RecorderError {
            code: first.code,
            message: format!(
                "{} Teardown also reported: {}",
                first.message, second.message
            ),
            recovery: first.recovery,
            action: format!("{} Then: {}", first.action, second.action),
        }),
        (Some(first), None) => Some(first),
        (None, second) => second,
    }
}

// ---------------------------------------------------------------- public model

/// Everything one live A/V recording needs before it starts.
///
/// The request is explicit by construction: the display identity, the encoder
/// configuration at the display's native resolution, the caller's audio routes and their
/// frozen plan, the one shared clock, the one shared origin and a *new* output path are
/// all supplied by the caller. A display/configuration mismatch, an ignored route or a
/// plan that disagrees with its routes is rejected before any native session starts.
pub struct AvRecordingRequest {
    /// The display to capture as a whole, at [`FpsMode::AutoMatchSource`].
    pub display_id: DisplayId,
    /// Exact selected WGC window; when present no display capture is opened.
    pub window_id: Option<frame_contracts::WindowId>,
    /// The hardware encoder configuration, at the display's native resolution.
    pub config: NativeEncoderConfig,
    /// Frozen policy selection, checked against the first real capture texture.
    pub encoder_id: Option<String>,
    /// The explicitly selected audio routes. An `Ignore` route is refused.
    pub routes: Vec<AudioRouteSelection>,
    /// The track plan those routes freeze, as [`frame_audio::plan_tracks`] returned it. It
    /// is checked against `routes` and never re-derived.
    pub plan: Vec<TrackPlan>,
    /// The one shared session clock, injected into the capture session and every audio
    /// source.
    pub clock: Arc<MasterClock>,
    /// The shared session origin the container subtracts from every packet timestamp.
    pub origin: MediaTimestamp,
    /// The output container path. It must not exist: a recording never overwrites one.
    pub output: PathBuf,
    pub meter: Arc<MeterHub>,
    pub meter_session: String,
    pub camera: Option<frame_contracts::CameraSelection>,
    pub show_cursor: bool,
    /// Frozen capability-selected FP16 scRGB -> NVENC HEVC Main10 path.
    pub hdr_main10: bool,
}

impl std::fmt::Debug for AvRecordingRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AvRecordingRequest")
            .field("display_id", &self.display_id)
            .field("config", &self.config)
            .field("routes", &self.routes)
            .field("plan", &self.plan)
            .field("origin", &self.origin)
            .field("output", &self.output)
            .finish_non_exhaustive()
    }
}

/// The lifecycle phase of one live A/V recording.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AvRecordingPhase {
    /// Admission is open: new capture surfaces are read, live audio is polled and packets
    /// are interleaved.
    #[default]
    Recording,
    /// `request_stop` closed admission. The video end of stream, the audio drain, the Opus
    /// trim and the remaining packets are still in progress.
    Draining,
    /// Every stream ended, every packet was written and the interleave buffer is empty.
    /// [`AvRecording::finish`] now joins the workers and finalizes the container.
    Drained,
    /// Every resource was released. A successful `finish` produced the artifact; an
    /// `abort`, a failed `finish` or `Drop` released the same resources without one.
    Closed,
}

impl AvRecordingPhase {
    fn describe(self) -> &'static str {
        match self {
            Self::Recording => "recording",
            Self::Draining => "draining",
            Self::Drained => "drained",
            Self::Closed => "closed",
        }
    }
}

/// What one bounded [`AvRecording::poll`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AvRecordingProgress {
    pub phase: AvRecordingPhase,
    /// Capture surfaces consumed in a bounded batch.
    pub capture_frames: u32,
    /// Inputs handed to the video encoder in this bounded turn.
    pub frames_submitted: u32,
    /// Live audio quanta this poll encoded.
    pub audio_quanta: u32,
    /// Packets this poll handed to the interleave buffer.
    pub packets_queued: u32,
    /// Packets this poll wrote to the container.
    pub packets_written: u32,
}

/// Bounded elapsed-time accounting for one call site. `None` in the snapshot means
/// that stage was never reached; a measured zero remains `Some` with a call count.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AvStageTiming {
    pub calls: u64,
    pub total_ns: u128,
    pub peak_ns: u128,
}

impl AvStageTiming {
    fn observe(slot: &mut Option<Self>, elapsed: Duration) {
        let sample = slot.get_or_insert_with(Self::default);
        sample.calls = sample.calls.saturating_add(1);
        sample.total_ns = sample.total_ns.saturating_add(elapsed.as_nanos());
        sample.peak_ns = sample.peak_ns.max(elapsed.as_nanos());
    }
}

/// Timed parts of the control thread. Some stages overlap: for example Stop drain
/// polls include the individual audio, video and mux calls they make. These values
/// locate a slow call; their totals must not be summed into a second wall clock.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AvRecordingTiming {
    pub poll: Option<AvStageTiming>,
    pub mux_write: Option<AvStageTiming>,
    pub audio_source_poll: Option<AvStageTiming>,
    pub audio_encode: Option<AvStageTiming>,
    pub video_capture: Option<AvStageTiming>,
    pub video_submit: Option<AvStageTiming>,
    pub video_output: Option<AvStageTiming>,
    pub stop_request: Option<AvStageTiming>,
    pub stop_drain_poll: Option<AvStageTiming>,
    pub stop_encoder_drain: Option<AvStageTiming>,
    pub stop_audio_drain: Option<AvStageTiming>,
    pub stop_tail: Option<AvStageTiming>,
    pub finish_teardown: Option<AvStageTiming>,
    /// `MkvAvWriter::finalize`, including file flush and sync.
    pub mux_finalize: Option<AvStageTiming>,
    /// Wall time from accepted Stop until both producers and the interleave drained.
    pub stop_to_drained_ns: Option<u128>,
    /// Wall time from accepted Stop until the finalized artifact was returned.
    pub stop_to_artifact_ns: Option<u128>,
}

/// One snapshot of what this live A/V recording delivered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AvRecordingStats {
    pub phase: AvRecordingPhase,
    pub timing: AvRecordingTiming,
    /// True once every announced track delivered its first packet. No packet is written to
    /// the container before that.
    pub ready: bool,
    /// Wall-clock time from `start` to that first write.
    pub readiness_ms: u128,
    pub capture_frames: u64,
    /// Video inputs the encoder accepted. Never a fabricated or duplicated frame.
    pub accepted_frames: u64,
    /// Owned inputs the encoder returned under pressure; each was retried unchanged.
    pub backpressure_returns: u64,
    /// Unaccepted owner surfaces explicitly discarded at a failed segment.
    pub restart_discarded_owner_frames: u64,
    /// Borrowed preparation pictures older than the fresh Opus segment origin.
    pub restart_stale_preparation_frames: u64,
    pub failed_submit_frames: u64,
    pub video_packets: u64,
    pub video_keyframes: u64,
    pub video_bytes: u64,
    /// The first and last source timestamps handed to the video encoder, unmodified.
    pub first_video_pts_ns: Option<u64>,
    pub last_video_pts_ns: Option<u64>,
    /// The first and last audio timestamps the Opus encoders produced, unmodified.
    pub first_audio_pts_ns: Option<u64>,
    pub last_audio_pts_ns: Option<u64>,
    pub audio_packets: u64,
    pub audio_bytes: u64,
    /// Live `BlockDuration` values the container writer patched with a measured interval.
    pub video_duration_patches: u64,
    /// Packets this recording handed to the container writer, by stream.
    pub video_packets_written: u64,
    pub audio_packets_written: u64,
    /// The real Opus discard padding of every finished track, in container track order.
    pub audio_trims: Vec<u16>,
    pub audio_input_frames: Vec<u64>,
    pub audio_written_by_track: Vec<u64>,
    /// Deepest the interleave buffer ever was, in packets and in payload bytes.
    pub peak_pending_packets: u64,
    pub peak_pending_bytes: u64,
    /// Furthest timestamp observed on any track: the delivery gate is measured from it.
    pub max_observed_pts_ns: Option<u64>,
    /// Times a delivery was held back only because the delivery delay had not passed.
    pub delayed_deliveries: u64,
    /// Deliberately stale packets a diagnostic run injected; always zero in production.
    pub injected_packets: u64,
    /// True while a diagnostic run is holding the live audio stream back.
    pub audio_choked: bool,
    /// The last real interval between two capture surfaces, kept as the video tail
    /// duration at the Stop boundary.
    pub last_video_interval_ns: Option<u64>,
    /// Stop boundary: the furthest timestamp observed when `request_stop` ran.
    pub stop_pts_ns: Option<u64>,
    /// Wall-clock time from `request_stop` to now, while draining or after finishing.
    pub drain_ms: u128,
    /// Everything the container writer still retained at the last snapshot.
    pub retained_bytes_estimate: u64,
}

/// The resources preparation created, before any of them is owned by a recording.
struct Prepared {
    session: Capture,
    encoder: WindowsH264Encoder,
    audio: EncodedAudioPipeline<NormalizedAudio>,
    writer: MkvAvWriter,
    first: Option<CapturedFrame<WindowsSurface>>,
}

/// Diagnostic switches of one run. All of them are off in production.
///
/// They exist so the accepted error paths of this owner — a video packet stale on its
/// own track and an interleave budget that is exceeded — can be provoked against
/// real capture instead of only against an injected unit-test writer. None of them can
/// change how a normal recording behaves.
#[derive(Debug, Default, Clone, Copy)]
pub struct Harness {
    /// Queue a deliberate stale video packet this many milliseconds after `start`.
    /// `None` queues none.
    pub stale_packet_after: Option<Duration>,
    /// How far behind the last observed timestamp that packet's PTS is.
    pub stale_packet_behind: Duration,
    /// Shrink the interleave packet budget, so a diagnostic run can trip it.
    pub interleave_packet_budget: Option<usize>,
    /// Hold back every live audio packet for this long after `start`, so a deliberately
    /// stuck stream fills the interleave buffer above its budget.
    pub choke_audio_for: Option<Duration>,
}

impl Harness {
    /// True while a live audio packet must still be held back.
    fn audio_choked(&self, elapsed: Duration) -> bool {
        self.choke_audio_for.is_some_and(|window| elapsed < window)
    }
}

// ------------------------------------------------------------- interleave queue

/// Which encoder a queued packet came from, resolved when it was queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PacketOrigin {
    Video,
    /// The plan index of the audio track, which is also the packet's track id.
    Audio(usize),
}

/// One packet waiting for its delivery delay inside the bounded interleave buffer.
#[derive(Debug)]
struct QueuedPacket {
    packet: EncodedPacket,
    origin: PacketOrigin,
    /// A monotonic insertion counter: two packets that share a PTS and a track keep their
    /// arrival order, so delivery is reproducible.
    sequence: u64,
}

impl PartialEq for QueuedPacket {
    fn eq(&self, other: &Self) -> bool {
        (self.packet.pts.0, self.packet.track_id, self.sequence)
            == (other.packet.pts.0, other.packet.track_id, other.sequence)
    }
}

impl Eq for QueuedPacket {}

impl Ord for QueuedPacket {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.packet.pts.0, self.packet.track_id, self.sequence).cmp(&(
            other.packet.pts.0,
            other.packet.track_id,
            other.sequence,
        ))
    }
}

impl PartialOrd for QueuedPacket {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The bounded interleave buffer: a min-heap in container order, not a reordering queue.
///
/// It holds only packets that arrived on time and are waiting for the delivery delay to
/// pass. Both budgets are explicit, and a breach is a terminal error, never a silent drop
/// or a re-timed packet.
#[derive(Debug, Default)]
struct Interleave {
    heap: BinaryHeap<std::cmp::Reverse<QueuedPacket>>,
    bytes: u64,
    sequence: u64,
    peak_packets: u64,
    peak_bytes: u64,
    /// The packet ceiling. It is the declared production budget except in a diagnostic
    /// run that asks for a smaller one so the budget path itself can be exercised.
    packet_budget: usize,
}

impl Interleave {
    /// The production buffer: the declared packet and byte budgets.
    #[cfg(test)]
    fn new() -> Self {
        Self {
            packet_budget: MAX_INTERLEAVE_PACKETS,
            ..Self::default()
        }
    }

    /// The same buffer with a smaller packet ceiling, for a diagnostic run that proves the
    /// budget path without waiting for a real unbounded queue.
    fn with_packet_budget(packet_budget: usize) -> Self {
        Self {
            packet_budget,
            ..Self::default()
        }
    }

    /// Adds one packet, charging both budgets before anything else happens.
    fn push(&mut self, packet: EncodedPacket, origin: PacketOrigin) -> Result<(), RecorderError> {
        let bytes = packet.data.len() as u64;
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| internal_error("The interleave sequence counter overflowed.".into()))?;
        let packets = self.heap.len() as u64 + 1;
        if packets > self.packet_budget as u64 {
            return Err(budget_error(format!(
                "The interleave buffer would hold {packets} packets, above the \
                 {}-packet budget; the stream that should have consumed them is not progressing.",
                self.packet_budget
            )));
        }
        let total = self.bytes.saturating_add(bytes);
        if total > MAX_INTERLEAVE_BYTES {
            return Err(budget_error(format!(
                "The interleave buffer would hold {total} bytes, above the \
                 {MAX_INTERLEAVE_BYTES}-byte budget; the stream that should have consumed them \
                 is not progressing."
            )));
        }
        self.sequence = sequence;
        self.bytes = total;
        self.heap.push(std::cmp::Reverse(QueuedPacket {
            packet,
            origin,
            sequence,
        }));
        self.peak_packets = self.peak_packets.max(packets);
        self.peak_bytes = self.peak_bytes.max(self.bytes);
        Ok(())
    }

    /// The earliest packet's PTS, if one is waiting.
    fn earliest_pts_ns(&self) -> Option<u64> {
        self.heap.peek().map(|entry| entry.0.packet.pts.0)
    }

    /// Which encoder the earliest packet came from, if one is waiting.
    fn earliest_origin(&self) -> Option<PacketOrigin> {
        self.heap.peek().map(|entry| entry.0.origin)
    }

    /// The earliest packet, oldest-first for equal timestamps, taken out of the queue.
    fn pop(&mut self) -> Option<QueuedPacket> {
        let entry = self.heap.pop()?;
        self.bytes = self.bytes.saturating_sub(entry.0.packet.data.len() as u64);
        Some(entry.0)
    }

    fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    fn len(&self) -> usize {
        self.heap.len()
    }

    fn bytes(&self) -> u64 {
        self.bytes
    }
}

// -------------------------------------------------------------- the recording

/// One running live A/V recording.
///
/// It owns the display capture session, the video encoder, the encoded audio pipeline and
/// the live container writer, and exactly one control thread drives it. `poll` bounds
/// native work and writes synchronously; `finish` joins the two video workers on the order
/// the shared D3D11 device requires and finalizes the container.
pub struct AvRecording {
    replay: frame_mux::replay::ReplayBuffer,
    disk_replay: Option<frame_mux::replay::RollingReplayStore>,
    replay_only: bool,
    pause_timeline: Arc<Mutex<PauseTimeline>>,
    paused: bool,
    display_id: DisplayId,
    config: NativeEncoderConfig,
    plan: Vec<TrackPlan>,
    clock: Arc<MasterClock>,
    origin: MediaTimestamp,
    output: PathBuf,
    session: Option<Capture>,
    admission: FrameAdmitter,
    encoder: Option<WindowsH264Encoder>,
    audio: Option<EncodedAudioPipeline<NormalizedAudio>>,
    writer: Option<MkvAvWriter>,
    session_index: Option<frame_mux::RecordingSession>,
    metadata: Option<crate::metadata::RecordingMetadata>,
    metadata_target: CaptureTarget,
    metadata_cursor_baked: bool,
    restart_budget: crate::encoder_restart::RestartBudget,
    segment_audio_written: Vec<u64>,
    segment_origin: MediaTimestamp,
    phase: AvRecordingPhase,
    stop_requested: bool,
    eos_requested: bool,
    drain_requested: bool,
    audio_drained: bool,
    /// True once the video encoder acknowledged its end of stream.
    video_encoder_drained: bool,
    /// The one-shot trim metadata of every declared track, held between the encoder flush
    /// and the moment the last block of each track is in the file.
    audio_trims_taken: Option<Vec<OpusTrackFinish>>,
    drain_tail_written: bool,
    closed: bool,
    failure: Option<RecorderError>,
    stats: AvRecordingStats,
    /// The one owned input the video encoder has not accepted yet. Under backpressure this
    /// is the *same* input that was offered, never a replacement.
    pending_input: Option<EncoderInput<CapturedFrame<WindowsSurface>>>,
    /// The previous capture surface's timestamp, only for the measured interval handed to
    /// the video encoder as the previous frame's duration. Nothing is held back.
    previous_frame_pts: Option<MediaTimestamp>,
    /// Diagnostic switches of this run. Every one of them is off in production.
    harness: Harness,
    /// True once the one diagnostic stale packet was injected.
    stale_packet_injected: bool,
    /// Packets waiting for their delivery delay, bounded by the two budgets.
    interleave: Interleave,
    /// One flag per announced audio track: it delivered its first packet.
    audio_firsts: Vec<bool>,
    video_first: bool,
    /// The furthest timestamp observed on any track. The ordinary delivery delay
    /// is measured back from it; another track may still deliver an earlier PTS.
    max_observed_pts_ns: Option<u64>,
    recording_started: Instant,
    drain_started: Option<Instant>,
    last_progress: Instant,
    last_progress_marker: u64,
    /// Device-free producer outputs; absent from production builds.
    #[cfg(test)]
    lifecycle_script: Option<lifecycle_tests::ProducerScript>,
}

impl std::fmt::Debug for AvRecording {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AvRecording")
            .field("display_id", &self.display_id)
            .field("output", &self.output)
            .field("phase", &self.phase)
            .field("closed", &self.closed)
            .field("audio_tracks", &self.plan.len())
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl AvRecording {
    /// Prepares one live A/V recording and returns it ready to accept media.
    ///
    /// Preparation is bounded and real: it opens every selected audio route through the
    /// accepted audio pipeline, waits at most [`PREPARATION_WATCHDOG`] for the first
    /// completed display surface, opens the hardware encoder on that surface's own Direct3D
    /// device and creates the live container from the encoder's own stream description and
    /// the plan's own real Opus pre-skip values. No frame is dropped, duplicated or rebased,
    /// and every failure path releases what it already created — encoder, capture session,
    /// audio sessions and the unwritten container — before returning.
    pub fn set_audio_muted(
        &mut self,
        id: &frame_contracts::AudioSourceId,
        muted: bool,
    ) -> Result<(), RecorderError> {
        self.audio.as_mut().map_or(Ok(()), |audio| {
            if audio
                .tracks()
                .iter()
                .any(|track| track.sources.contains(id))
            {
                audio.set_muted(id, muted)
            } else {
                Ok(())
            }
        })
    }

    pub fn start(request: AvRecordingRequest) -> Result<Self, RecorderError> {
        Self::start_with_selection(request, None, FrameAdmitter::auto())
    }

    /// Starts the A/V owner with a canonical region and a frozen FPS admission policy.
    pub fn start_with_selection(
        request: AvRecordingRequest,
        region: Option<frame_contracts::Region>,
        admission: FrameAdmitter,
    ) -> Result<Self, RecorderError> {
        Self::start_with_preview(request, region, admission, None)
    }

    /// Attaches a thumbnail hub to the same display capture that feeds the A/V owner.
    pub fn start_with_preview(
        request: AvRecordingRequest,
        region: Option<frame_contracts::Region>,
        admission: FrameAdmitter,
        preview: Option<&Arc<frame_windows::preview::PreviewHub>>,
    ) -> Result<Self, RecorderError> {
        Self::start_with_preview_cancellable(
            request,
            region,
            admission,
            preview,
            &AtomicBool::new(false),
        )
    }

    /// Cancellation is checked between stages; native calls must return before cleanup.
    pub(crate) fn start_with_preview_cancellable(
        request: AvRecordingRequest,
        region: Option<frame_contracts::Region>,
        mut admission: FrameAdmitter,
        preview: Option<&Arc<frame_windows::preview::PreviewHub>>,
        cancel_start: &AtomicBool,
    ) -> Result<Self, RecorderError> {
        macro_rules! checkpoint {
            () => {
                crate::recording_control::check_preparation_cancelled(cancel_start)?;
            };
            ($($cleanup:expr),+ $(,)?) => {
                if let Err(error) = crate::recording_control::check_preparation_cancelled(cancel_start) {
                    let mut failure = Some(error);
                    $(failure = combine_errors(failure, $cleanup.err());)*
                    return Err(failure.expect("cancellation"));
                }
            };
        }
        checkpoint!();
        crate::video_recording::validate_encoder_signal(
            request.hdr_main10,
            request.encoder_id.as_deref(),
        )?;
        if request.window_id.is_some()
            && (region.is_some() || request.hdr_main10 || request.camera.is_some())
        {
            return Err(invalid_request_error(
                "Window capture uses SDR native video without region or camera composition.".into(),
            ));
        }
        if request.hdr_main10 && (region.is_some() || request.camera.is_some() || preview.is_some())
        {
            return Err(invalid_request_error(
                "HDR Main10 currently supports whole-display capture without region, camera or preview.".into(),
            ));
        }
        if region
            .as_ref()
            .is_some_and(|region| region.display_id != request.display_id)
        {
            return Err(invalid_request_error(
                "The region and recording request name different displays.".into(),
            ));
        }
        request.config.validate()?;
        if let Some(hub) = preview {
            hub.set_recording();
        }
        if request.output.exists() {
            return Err(RecorderError {
                code: ErrorCode::InvalidOutputPath,
                message: format!(
                    "The output path {} already exists; a recording never overwrites an existing file.",
                    request.output.display()
                ),
                recovery: RecoveryStatus::NoRecording,
                action: "Choose a new output file path inside an existing writable directory."
                    .to_string(),
            });
        }
        // The plan the caller froze is the plan this recording declares; re-deriving it
        // later would let a route change slip in between the two.
        let derived = frame_audio::plan_tracks(&request.routes)?;
        if derived != request.plan {
            return Err(invalid_request_error(
                "The supplied track plan does not match the supplied audio routes; the container would declare a different set of tracks than the mixer produces."
                    .to_string(),
            ));
        }
        if request.routes.is_empty() || request.plan.is_empty() {
            return Err(invalid_request_error(
                "A live A/V recording needs at least one selected audio route and one planned track."
                    .to_string(),
            ));
        }
        if request
            .routes
            .iter()
            .any(|route| route.route == AudioRoute::Ignore)
        {
            return Err(invalid_request_error(
                "An ignored audio route cannot be part of a recording request; remove it instead of routing it to Ignore."
                    .to_string(),
            ));
        }

        let metadata_target = if let Some(id) = &request.window_id {
            CaptureTarget::Window(id.clone())
        } else if let Some(region) = &region {
            CaptureTarget::Region(region.clone())
        } else {
            CaptureTarget::Display(request.display_id.clone())
        };
        let started = Instant::now();
        // 1. Prepare display capture before starting bounded audio queues. DXGI
        // preparation can exceed the microphone's 32-block queue window; starting
        // audio first would turn a valid device into a deterministic overflow.
        checkpoint!();
        let context = CaptureContext {
            clock: Arc::clone(&request.clock),
            cancellation: CaptureCancellation::default(),
        };
        let mut backend = PreparationAudioBackend {
            backend: NormalizingBackend::with_policy(
                PlatformBackend,
                frame_audio::AudioContinuityPolicy::from_environment()?,
            ),
            cancel: cancel_start,
        };
        let capture_result: Result<Capture, RecorderError> = if let Some(window_id) =
            &request.window_id
        {
            frame_windows::WindowsWindowCaptureSession::start_selected_with_preview_cursor(
                window_id,
                FpsMode::AutoMatchSource,
                context,
                preview.cloned(),
                request.show_cursor,
            )
            .map(|capture| Box::new(capture) as Capture)
        } else {
            match region {
                Some(region) => {
                    let capture = if let Some(hub) = preview.filter(|_| request.camera.is_none()) {
                        frame_windows::RegionCaptureSession::start_with_preview_cursor(
                            &region,
                            context,
                            Arc::clone(hub),
                            request.show_cursor,
                        )
                    } else {
                        frame_windows::RegionCaptureSession::start_with_cursor(
                            &region,
                            context,
                            request.show_cursor,
                        )
                    };
                    capture.map(|capture| Box::new(capture) as Capture)
                }
                None => {
                    let target = CaptureTarget::Display(request.display_id.clone());
                    if request.camera.is_none() {
                        frame_windows::WindowsCaptureSession::prepare_recording_display(
                            &request.display_id,
                            context,
                            request.show_cursor,
                            request.hdr_main10,
                            preview.cloned(),
                        )
                        .map(|capture| Box::new(capture) as Capture)
                    } else {
                        frame_windows::start_display_with_cursor(
                            &target,
                            FpsMode::AutoMatchSource,
                            context,
                            request.show_cursor,
                        )
                        .map(|capture| Box::new(capture) as Capture)
                    }
                }
            }
        };
        let mut capture = capture_result?;
        checkpoint!(capture.stop());
        if let Some(choice) = &request.camera {
            capture = Box::new(frame_windows::compositor::CameraCaptureSession::start(
                capture,
                choice,
                Arc::clone(&request.clock),
                request.config.width,
                request.config.height,
                preview.cloned(),
            )?);
        }
        // 2. Start audio once the display session is ready. The shared clock/origin
        // still preserves the real startup offset, and the first-frame wait below
        // drains native audio while it waits for a surface.
        checkpoint!(capture.stop());
        let mut pipeline = AudioPipeline::start(
            &mut backend,
            &request.routes,
            Arc::clone(&request.clock),
            request.origin,
        )
        .map_err(|error| combine_errors(Some(error), capture.stop().err()).expect("primary"))?;
        pipeline.attach_meter(Arc::clone(&request.meter), request.meter_session.clone());
        checkpoint!(capture.stop(), pipeline.stop());
        let mut audio = EncodedAudioPipeline::new(pipeline, request.origin)
            .map_err(|error| combine_errors(Some(error), capture.stop().err()).expect("primary"))?;
        checkpoint!(capture.stop(), audio.stop());
        let pause_timeline = Arc::new(Mutex::new(PauseTimeline::default()));
        audio.set_pause_timeline(Arc::clone(&pause_timeline));
        if audio.origin() != request.origin {
            let error = internal_error(
                "The encoded audio pipeline was built with a different media origin.".to_string(),
            );
            let failure = combine_errors(Some(error), audio.stop().err());
            return Err(combine_errors(failure, capture.stop().err()).expect("primary"));
        }
        let preparation_surface = match capture.preparation_surface() {
            Ok(surface) => surface,
            Err(error) => {
                let failure = combine_errors(Some(error), capture.stop().err());
                let failure = combine_errors(failure, audio.stop().err()).expect("primary");
                return Err(failure);
            }
        };
        let (first, prepared_audio) = if preparation_surface.is_some() {
            (None, Vec::new())
        } else {
            match wait_for_first_surface(capture.as_mut(), &mut audio, &request.clock, cancel_start)
            {
                Ok((first, packets)) => (Some(first), packets),
                Err(error) => {
                    let failure = combine_errors(Some(error), capture.stop().err());
                    return Err(combine_errors(failure, audio.stop().err()).expect("primary"));
                }
            }
        };
        let surface = preparation_surface
            .as_ref()
            .or_else(|| first.as_ref().map(|frame| &frame.surface))
            .expect("prepared backing or actual first frame");
        checkpoint!(capture.stop(), audio.stop());
        let opened = match request.encoder_id.as_deref() {
            Some(id) => WindowsH264Encoder::open_selected(request.config, surface, id),
            None if request.hdr_main10 => {
                WindowsH264Encoder::open_hevc_main10(request.config, surface)
            }
            None => WindowsH264Encoder::open(request.config, surface),
        };
        let mut encoder = match opened {
            Ok(encoder) => encoder,
            Err(error) => {
                let failure = combine_errors(Some(error), capture.stop().err());
                let failure = combine_errors(failure, audio.stop().err()).expect("primary");
                return Err(failure);
            }
        };
        drop(preparation_surface);
        checkpoint!(
            crate::video_recording::stop_preparation_encoder(&mut encoder),
            capture.stop(),
            audio.stop()
        );
        if let Some(first) = &first
            && let Err(error) = admission.admit(first.timestamp)
        {
            let failure = combine_errors(Some(error), encoder.stop().err());
            let failure = combine_errors(failure, capture.stop().err());
            return Err(combine_errors(failure, audio.stop().err()).expect("primary"));
        }

        // 3. The container, from the video encoder's own stream description and the audio
        //    encoders' own pre-skip values, which are readable before any PCM arrives.
        let mut tracks = Vec::with_capacity(request.plan.len());
        for (index, track) in request.plan.iter().enumerate() {
            let Some(pre_skip) = audio.pre_skip(index) else {
                let error = internal_error(format!(
                    "The Opus encoder of planned track {index} was not created, so the container cannot declare it."
                ));
                let failure = combine_errors(Some(error), encoder.stop().err());
                let failure = combine_errors(failure, capture.stop().err());
                let failure = combine_errors(failure, audio.stop().err()).expect("primary");
                return Err(failure);
            };
            match AvAudioTrack::from_plan(track, pre_skip)
                .and_then(|track| track.with_channels(audio.track_channels(index)))
            {
                Ok(track) => tracks.push(track),
                Err(error) => {
                    let failure = combine_errors(Some(error), encoder.stop().err());
                    let failure = combine_errors(failure, capture.stop().err());
                    let failure = combine_errors(failure, audio.stop().err()).expect("primary");
                    return Err(failure);
                }
            }
        }
        let replay = frame_mux::replay::ReplayBuffer::new(
            encoder.stream_info().clone(),
            tracks.clone(),
            frame_mux::replay::ReplayLimit {
                duration_ns: 30_000_000_000,
                max_bytes: 64 * 1024 * 1024,
            },
        )
        .map_err(|error| {
            let failure = combine_errors(Some(error), encoder.stop().err());
            let failure = combine_errors(failure, capture.stop().err());
            combine_errors(failure, audio.stop().err()).expect("primary")
        })?;
        checkpoint!(
            crate::video_recording::stop_preparation_encoder(&mut encoder),
            capture.stop(),
            audio.stop()
        );
        let restart_budget = crate::encoder_restart::RestartBudget::from_environment()?;
        let indexed_writer = (|| {
            let mut index = frame_mux::RecordingSession::create(
                &crate::encoder_restart::manifest_path(&request.output),
            )?;
            let writer =
                index.create_av_segment(encoder.stream_info(), &tracks, request.origin, true)?;
            Ok::<_, RecorderError>((index, writer))
        })();
        let (session_index, writer) = match indexed_writer {
            Ok(prepared) => prepared,
            Err(error) => {
                let failure = combine_errors(Some(error), encoder.stop().err());
                let failure = combine_errors(failure, capture.stop().err());
                let failure = combine_errors(failure, audio.stop().err()).expect("primary");
                return Err(failure);
            }
        };

        let prepared = Prepared {
            session: capture,
            encoder,
            audio,
            writer,
            first,
        };
        let audio_tracks = request.plan.len();
        let now = Instant::now();
        let stats = AvRecordingStats {
            phase: AvRecordingPhase::Recording,
            capture_frames: u64::from(prepared.first.is_some()),
            first_video_pts_ns: prepared.first.as_ref().map(|first| first.timestamp.0),
            audio_trims: vec![0; audio_tracks],
            audio_input_frames: vec![0; audio_tracks],
            audio_written_by_track: vec![0; audio_tracks],
            ..AvRecordingStats::default()
        };
        let first_pts = prepared.first.as_ref().map(|first| first.timestamp);
        let mut recording = Self {
            replay,
            disk_replay: None,
            replay_only: false,
            pause_timeline,
            paused: false,
            display_id: request.display_id,
            config: request.config,
            plan: request.plan,
            clock: request.clock,
            origin: request.origin,
            output: request.output,
            session: Some(prepared.session),
            admission,
            encoder: Some(prepared.encoder),
            audio: Some(prepared.audio),
            writer: Some(prepared.writer),
            session_index: Some(session_index),
            metadata: None,
            metadata_target: metadata_target.clone(),
            metadata_cursor_baked: request.show_cursor,
            restart_budget,
            segment_audio_written: vec![0; audio_tracks],
            segment_origin: request.origin,
            phase: AvRecordingPhase::Recording,
            stop_requested: false,
            eos_requested: false,
            drain_requested: false,
            audio_drained: false,
            video_encoder_drained: false,
            audio_trims_taken: None,
            drain_tail_written: false,
            closed: false,
            failure: None,
            stats,
            // The surface that opened the encoder is this session's first input and is
            // submitted by the first poll; nothing is held back for a lookahead.
            pending_input: prepared.first.map(|first| EncoderInput {
                input: first,
                duration_ns: nominal_duration(request.config.rate_num, request.config.rate_den),
            }),
            previous_frame_pts: first_pts,
            interleave: Interleave::with_packet_budget(
                MAX_INTERLEAVE_PACKETS.max(audio_tracks * 32),
            ),
            harness: Harness::default(),
            stale_packet_injected: false,
            audio_firsts: vec![false; audio_tracks],
            video_first: false,
            max_observed_pts_ns: None,
            recording_started: started,
            drain_started: None,
            last_progress: now,
            last_progress_marker: 0,
            #[cfg(test)]
            lifecycle_script: None,
        };
        // The audio encoded while the display session was starting is handed to the
        // container now, so preparation loses no PCM and leaves no native queue full.
        if let Err(error) = recording.queue_audio_packets(prepared_audio) {
            return Err(combine_errors(Some(error), recording.abort().err()).expect("primary"));
        }
        if let Err(mut error) = crate::recording_control::check_preparation_cancelled(cancel_start)
        {
            error.recovery = RecoveryStatus::Unknown;
            return Err(
                combine_errors(Some(error), recording.abort_preparation().err())
                    .expect("cancellation"),
            );
        }
        recording.attach_pipeline_wake();
        if crate::metadata::enabled()
            && let Err(error) = recording
                .enable_metadata(metadata_target, frame_metadata::MetadataOptions::default())
        {
            return Err(
                combine_errors(Some(error), recording.abort().err()).expect("metadata startup")
            );
        }
        frame_windows::cursor_preview_diagnostics::record(
            "recording_prepared",
            None,
            || serde_json::json!({"media_ns": recording.clock.now().0, "audio": true}),
        );
        Ok(recording)
    }

    /// Explicit local opt-in, called on the control owner before its first poll.
    /// Environment opt-in uses the same path. Metadata remains off by default.
    pub fn enable_metadata_for_source(
        &mut self,
        options: frame_metadata::MetadataOptions,
    ) -> Result<(), RecorderError> {
        self.enable_metadata(self.metadata_target.clone(), options)
    }
    pub fn enable_metadata(
        &mut self,
        target: CaptureTarget,
        options: frame_metadata::MetadataOptions,
    ) -> Result<(), RecorderError> {
        if target != self.metadata_target {
            return Err(invalid_request_error(
                "Metadata must observe the actual selected recording source.".into(),
            ));
        }
        if self.metadata.is_some()
            || self.closed
            || self.stop_requested
            || self.paused
            || self.stats.video_packets_written + self.stats.audio_packets_written != 0
        {
            return Err(invalid_transition_error(
                "Enable metadata once before recording admission.".into(),
            ));
        }
        let index = self
            .session_index
            .as_mut()
            .ok_or_else(|| internal_error("Missing metadata session index.".into()))?;
        self.metadata = Some(crate::metadata::RecordingMetadata::start(
            index,
            self.clock.clone(),
            self.pause_timeline.clone(),
            self.origin,
            target,
            self.metadata_cursor_baked,
            options,
        )?);
        Ok(())
    }

    fn finish_metadata(&mut self, completed: bool) -> Result<(), RecorderError> {
        if let Some(mut metadata) = self.metadata.take() {
            metadata.finish(completed)?;
        }
        Ok(())
    }

    /// The display this recording captures.
    pub fn display_id(&self) -> &DisplayId {
        &self.display_id
    }

    /// The requested encoder configuration.
    pub fn config(&self) -> NativeEncoderConfig {
        self.config
    }

    /// Arms the diagnostic switches of this run. Call before the first `poll`.
    ///
    /// The interleave packet budget cannot be raised above the declared
    /// [`MAX_INTERLEAVE_PACKETS`] and cannot be zero: a diagnostic run may only narrow the
    /// production budget, never widen it.
    pub fn set_harness(&mut self, harness: Harness) -> Result<(), RecorderError> {
        if let Some(budget) = harness.interleave_packet_budget {
            if budget == 0 || budget > MAX_INTERLEAVE_PACKETS {
                return Err(invalid_request_error(format!(
                    "The diagnostic interleave budget must be 1..={MAX_INTERLEAVE_PACKETS}, not {budget}."
                )));
            }
            self.interleave = Interleave::with_packet_budget(budget);
        }
        self.harness = harness;
        Ok(())
    }

    /// Queues one deliberate video packet whose PTS is `behind_ns` before the furthest
    /// timestamp already observed, so the video track's own order check can be
    /// exercised against real capture when that PTS is behind written video.
    ///
    /// It is a diagnostic entry point for that error path, never part of a recording: it
    /// moves no clock and shifts no packet. A run that uses it is expected to end in an
    /// explicit terminal error and to leave no artifact.
    pub fn inject_stale_video_packet(&mut self, behind_ns: u64) -> Result<(), RecorderError> {
        if let Some(error) = self.failure.clone() {
            return Err(error);
        }
        if self.closed || self.stop_requested {
            return Err(invalid_transition_error(
                "A stale packet can only be injected while the recording still admits new media."
                    .to_string(),
            ));
        }
        let Some(max) = self.max_observed_pts_ns else {
            return Err(invalid_transition_error(
                "No packet has been observed yet, so there is no frontier to fall behind."
                    .to_string(),
            ));
        };
        let Some(pts_ns) = max
            .checked_sub(behind_ns)
            .filter(|pts| *pts > self.origin.0)
        else {
            return Err(invalid_request_error(format!(
                "A packet {behind_ns} ns behind timestamp {max} ns would precede the shared origin {} ns.",
                self.origin.0
            )));
        };
        // A well-formed Annex-B access unit, so the injected packet reaches the
        // container's own order check instead of failing earlier for another reason.
        let packet = EncodedPacket {
            track_id: self.plan.len() as u32,
            pts: MediaTimestamp(pts_ns),
            dts: MediaTimestamp(pts_ns),
            duration_ns: 1_000_000,
            keyframe: false,
            data: vec![0, 0, 0, 1, 0x65, 0x88, 0x84, 0x21, 0x00],
        };
        self.stats.injected_packets += 1;
        self.queue_video_packet(packet)
    }

    /// The sticky terminal failure this recording reported, by code alone.
    pub fn failure_code(&self) -> Option<ErrorCode> {
        self.failure.as_ref().map(|error| error.code)
    }

    /// The frozen audio track plan, in container order.
    pub fn plan(&self) -> &[TrackPlan] {
        &self.plan
    }

    /// The shared session origin the container subtracts from every packet timestamp.
    pub fn origin(&self) -> MediaTimestamp {
        self.origin
    }

    /// The output container path.
    pub fn output_path(&self) -> &Path {
        &self.output
    }

    /// The effective native encoder configuration, as the driver reported it.
    pub fn encoder_settings(&self) -> Option<&NativeEncoderSettings> {
        self.encoder.as_ref().map(|encoder| encoder.settings())
    }

    /// The writer's own counters, for container evidence.
    pub fn writer_stats(&self) -> Option<frame_mux::MkvAvWriterStats> {
        self.writer.as_ref().map(MkvAvWriter::stats)
    }

    /// One snapshot of what this recording delivered.
    pub fn stats(&self) -> AvRecordingStats {
        self.stats.clone()
    }

    /// Raw source-clock estimates; independent from packet delivery and preview.
    pub fn audio_clock_health(&self) -> Vec<frame_audio::SourceClockHealth> {
        self.audio
            .as_ref()
            .map_or_else(Vec::new, |audio| audio.clock_health().collect())
    }

    pub fn audio_source_lifecycle(&self) -> Vec<frame_audio::SourceLifecycleSnapshot> {
        self.audio
            .as_ref()
            .map_or_else(Vec::new, |audio| audio.source_lifecycle().collect())
    }

    /// Owner-thread device replacement keeps the frozen logical route and MKV track.
    pub fn replace_audio_source_device(
        &mut self,
        logical: &frame_contracts::AudioSourceId,
        physical: &frame_contracts::AudioSourceId,
    ) -> Result<(), RecorderError> {
        if self.stats.phase != AvRecordingPhase::Recording {
            return Err(invalid_transition_error(
                "Device replacement requires a live recording.".into(),
            ));
        }
        let mut backend = NormalizingBackend::with_policy(
            PlatformBackend,
            frame_audio::AudioContinuityPolicy::from_environment()?,
        );
        self.audio
            .as_mut()
            .ok_or_else(|| invalid_transition_error("Audio source ownership has ended.".into()))?
            .replace_source_device(logical, physical, &mut backend)
    }

    fn attach_pipeline_wake(&mut self) {
        let wake = frame_capture::PipelineWake::for_current_thread();
        if let Some(session) = self.session.as_mut() {
            session.set_wake_signal(wake.clone());
        }
        if let Some(encoder) = self.encoder.as_mut() {
            encoder.set_wake_signal(wake);
        }
    }

    pub fn admission_counters(&self) -> frame_encode::admission::AdmissionCounters {
        self.admission.counters()
    }

    pub fn capture_pipeline_stats(&self) -> Option<frame_capture::CapturePipelineStats> {
        self.session.as_ref()?.pipeline_stats()
    }

    /// The native encoder's completed picture count for the desktop rate sampler.
    pub fn encoder_stats(&self) -> Option<frame_windows::EncoderSessionStats> {
        self.encoder.as_ref().map(WindowsH264Encoder::stats)
    }

    /// The sticky failure this recording reported, if any.
    pub fn failure(&self) -> Option<&RecorderError> {
        self.failure.as_ref()
    }

    pub(crate) fn replay_buffer(&self) -> &frame_mux::replay::ReplayBuffer {
        &self.replay
    }
    pub(crate) fn replay_metadata_path(&self) -> Option<PathBuf> {
        self.metadata.as_ref()?;
        Some(self.session_index.as_ref()?.metadata_path())
    }
    pub(crate) fn rolling_metadata(&self) -> Option<frame_metadata::RollingMetadata> {
        self.metadata.as_ref()?.rolling()
    }

    /// The finite startup prefix stays recoverable; only rolling segments grow.
    pub(crate) fn enable_replay_only(&mut self) -> Result<(), RecorderError> {
        if let Some(metadata) = self.metadata.as_mut() {
            metadata.enable_rolling(self.replay.limit().duration_ns)?;
        }
        self.replay_only = true;
        self.writer = None;
        self.session_index = None;
        Ok(())
    }

    pub fn configure_replay(
        &mut self,
        limit: frame_mux::replay::ReplayLimit,
    ) -> Result<(), RecorderError> {
        if self
            .disk_replay
            .as_ref()
            .is_some_and(|store| store.segment_duration_ns() > limit.duration_ns)
        {
            return Err(RecorderError::invalid(
                "Replay memory window must cover its disk segment.",
            ));
        }
        self.replay.set_limit(limit)?;
        if let Some(capture) = self.session.as_mut() {
            capture.set_replay_sampling(true);
        }
        if let Some(encoder) = self.encoder.as_mut() {
            encoder.set_replay_keyframes(true);
        }
        Ok(())
    }

    pub fn save_replay(
        &self,
        path: &Path,
        duration_ns: u64,
    ) -> Result<RecordingArtifact, RecorderError> {
        self.replay.save_session(path, duration_ns)
    }

    pub fn configure_disk_replay(
        &mut self,
        directory: &Path,
        limit: frame_mux::replay::DiskReplayLimit,
    ) -> Result<(), RecorderError> {
        if self.closed
            || self.stop_requested
            || self.disk_replay.is_some()
            || limit.segment_ns > self.replay.limit().duration_ns
        {
            return Err(RecorderError::invalid(
                "Disk replay cannot be replaced or exceed the memory window.",
            ));
        }
        self.disk_replay = Some(frame_mux::replay::RollingReplayStore::create(
            directory, limit,
        )?);
        Ok(())
    }
    pub fn disk_replay_stats(&self) -> Option<frame_mux::replay::DiskReplayStats> {
        self.disk_replay.as_ref().map(|store| store.stats())
    }
    pub fn export_disk_replay(
        &mut self,
        manifest: &Path,
    ) -> Result<frame_mux::SessionRecoverySummary, RecorderError> {
        self.disk_replay
            .as_mut()
            .ok_or_else(|| RecorderError::invalid("Disk replay is disabled."))?
            .export(manifest)
    }

    pub fn replay_bytes(&self) -> u32 {
        self.replay.retained_bytes() as u32
    }

    pub fn active_timestamp(&self, raw: MediaTimestamp) -> MediaTimestamp {
        self.pause_timeline
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .map(raw)
            .unwrap_or(raw)
    }

    pub fn pause(&mut self) -> Result<(), RecorderError> {
        if self.phase != AvRecordingPhase::Recording || self.paused {
            return Err(invalid_transition_error(
                "A/V recording is not running.".into(),
            ));
        }
        let mut timeline = self
            .pause_timeline
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(metadata) = &self.metadata {
            metadata.transition(&mut timeline, frame_metadata::Lifecycle::Paused)?;
        } else {
            timeline
                .pause(self.clock.now())
                .ok_or_else(|| internal_error("Pause timestamp failed.".into()))?;
        }
        drop(timeline);
        self.paused = true;
        Ok(())
    }

    pub fn resume(&mut self) -> Result<(), RecorderError> {
        if self.phase != AvRecordingPhase::Recording || !self.paused {
            return Err(invalid_transition_error(
                "A/V recording is not paused.".into(),
            ));
        }
        // A fast Pause→Resume may arrive before the owner's next poll. Consume the
        // bounded native PCM backlog while the pause boundary is still active so
        // blocks captured just before Pause retain their samples and later blocks
        // are clipped instead of being interpreted as post-resume input.
        self.service_audio_sources()?;
        let mut timeline = self
            .pause_timeline
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(metadata) = &self.metadata {
            metadata.transition(&mut timeline, frame_metadata::Lifecycle::Resumed)?;
        } else {
            timeline
                .resume(self.clock.now())
                .ok_or_else(|| internal_error("Resume timestamp failed.".into()))?;
        }
        drop(timeline);
        self.paused = false;
        self.last_progress = Instant::now();
        Ok(())
    }

    /// One bounded step of the recording.
    ///
    /// On the ordinary path each step reads at
    /// most MAX_PACKETS capture surfaces, submits a bounded video batch, encodes a bounded number
    /// of audio quanta and writes a bounded number of packets. A missing surface is not an
    /// error — a static desktop keeps a live session — while the absence of any progress
    /// *while work is owned* is reported once [`PROGRESS_WATCHDOG`] passed.
    ///
    /// Opt-in audio reconnect can prepare a native source. An opt-in encoder restart
    /// joins the failed worker and prepares a replacement while servicing audio. These
    /// preparation paths retain the native backend's timeout/cleanup limits.
    ///
    /// A returned error is sticky and terminal: `finish` reports the same failure and
    /// releases the resources, and `abort` (or `Drop`) does the same without an artifact.
    pub fn poll(&mut self) -> Result<AvRecordingProgress, RecorderError> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        if self.closed {
            return Err(invalid_transition_error(
                "This recording is closed: it was finished, aborted or dropped, so it can no longer produce media."
                    .to_string(),
            ));
        }
        let measured_phase = self.phase;
        let poll_started = Instant::now();
        let mut outcome = match self.phase {
            AvRecordingPhase::Recording if self.paused => self.poll_paused(),
            AvRecordingPhase::Recording => self.poll_recording(),
            AvRecordingPhase::Draining => self.poll_draining(),
            AvRecordingPhase::Drained => Ok(AvRecordingProgress {
                phase: AvRecordingPhase::Drained,
                ..AvRecordingProgress::default()
            }),
            AvRecordingPhase::Closed => Err(invalid_transition_error(
                "This recording is closed, so it can no longer produce media.".to_string(),
            )),
        };
        if let Err(error) = &outcome {
            let worker = self
                .encoder
                .as_ref()
                .and_then(WindowsH264Encoder::native_failure);
            if self.restart_budget.begin(
                !self.replay_only
                    && self.phase == AvRecordingPhase::Recording
                    && !self.paused
                    && !self.stop_requested,
                error,
                worker,
            ) {
                outcome = self
                    .restart_encoder_segment()
                    .map(|()| AvRecordingProgress {
                        phase: self.phase,
                        ..AvRecordingProgress::default()
                    });
            }
        }
        if outcome.is_ok()
            && self.phase == AvRecordingPhase::Recording
            && !self.paused
            && let Some(store) = self.disk_replay.as_mut()
            && let Err(error) = store.checkpoint_if_due(&self.replay)
        {
            outcome = Err(error);
        }
        if let Err(error) = &outcome {
            // Sticky. The resources stay owned by this value so that `finish`, `abort` or
            // `Drop` releases them on the control thread. Opt-in restart cleanup also
            // runs on this owner thread before a replacement encoder is prepared.
            self.failure.get_or_insert_with(|| error.clone());
        }
        if matches!(
            measured_phase,
            AvRecordingPhase::Recording | AvRecordingPhase::Draining
        ) {
            let elapsed = poll_started.elapsed();
            AvStageTiming::observe(&mut self.stats.timing.poll, elapsed);
            if measured_phase == AvRecordingPhase::Draining {
                AvStageTiming::observe(&mut self.stats.timing.stop_drain_poll, elapsed);
            }
        }
        self.refresh_stats();
        outcome
    }

    /// Closes admission permanently. Idempotent and non-blocking.
    ///
    /// No capture surface is read and no live audio quantum is encoded after this call. The
    /// inputs this session already owns are still delivered by `poll`, and only then does
    /// the video encoder receive its end of stream.
    pub fn request_stop(&mut self) -> Result<(), RecorderError> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        if self.closed {
            return Err(invalid_transition_error(
                "This recording is closed, so it cannot be asked to stop.".to_string(),
            ));
        }
        if self.stop_requested {
            return Ok(());
        }
        let stop_started = Instant::now();
        if self.paused {
            // Preserve PCM captured before the pause boundary before closing native
            // admission. The pipeline clips the last overlapping block itself.
            let paused_drain = (|| {
                self.service_audio_sources()?;
                self.audio
                    .as_mut()
                    .ok_or_else(|| {
                        internal_error("Paused recording has no audio pipeline.".into())
                    })?
                    .request_drain()
            })();
            if let Err(error) = paused_drain {
                // A failed pre-drain has already touched native audio state. Keep the
                // failure terminal so a second Stop cannot finalize a partial stream.
                self.failure = Some(error.clone());
                AvStageTiming::observe(&mut self.stats.timing.stop_request, stop_started.elapsed());
                return Err(error);
            }
            self.drain_requested = true;
        }
        if let Some(metadata) = &mut self.metadata {
            metadata.stopping()?;
        }
        self.stop_requested = true;
        self.phase = AvRecordingPhase::Draining;
        let now = Instant::now();
        self.drain_started = Some(now);
        // The drain budget is measured from this request, not from the last packet: an idle
        // desktop may have delivered nothing for minutes before stop.
        self.last_progress = now;
        self.last_progress_marker = self.progress_marker();
        self.stats.stop_pts_ns = Some(self.active_timestamp(self.clock.now()).0);
        AvStageTiming::observe(&mut self.stats.timing.stop_request, stop_started.elapsed());
        Ok(())
    }

    /// Completes the recording and produces the one artifact.
    ///
    /// It requires a completed drain (`poll` reported [`AvRecordingPhase::Drained`]): a
    /// stream that was not fully delivered is never presented as a finished one. The
    /// teardown order is video encoder stop, display capture stop, audio pipeline stop,
    /// container finalize. Any teardown failure releases every resource and returns
    /// *without* finalizing the container, so a failure never produces an artifact.
    pub fn finish(&mut self) -> Result<RecordingArtifact, RecorderError> {
        if self.closed {
            return Err(match &self.failure {
                Some(error) => error.clone(),
                None => invalid_transition_error(
                    "This recording was already finished; its artifact was produced once."
                        .to_string(),
                ),
            });
        }
        if let Some(error) = self.failure.clone() {
            let _ = self.release();
            return Err(error);
        }
        if self.phase != AvRecordingPhase::Drained {
            return Err(invalid_transition_error(format!(
                "The recording is {}: keep calling poll() until it reports drained, or abort it. A container is never finalized from a stream that was not fully delivered.",
                self.phase.describe()
            )));
        }
        self.refresh_stats();

        // 1. The video encoder is stopped first: destroying the hardware session is what the
        //    shared-device evidence requires before the capture worker is torn down.
        let teardown_started = Instant::now();
        let encoder_failure = match self.encoder.as_mut() {
            Some(encoder) => encoder.stop().err(),
            None => None,
        };
        // 2. Capture is stopped in every case, so a failed encoder teardown still releases
        //    the capture worker instead of leaving it behind.
        let capture_failure = match self.session.as_mut() {
            Some(session) => session.stop().err(),
            None => None,
        };
        // 3. The audio pipeline is already finished by the drain; this is the explicit
        //    idempotent stop that also releases a pipeline left behind by a failure.
        let audio_failure = match self.audio.as_mut() {
            Some(audio) => audio.stop().err(),
            None => None,
        };
        AvStageTiming::observe(
            &mut self.stats.timing.finish_teardown,
            teardown_started.elapsed(),
        );
        self.encoder = None;
        self.session = None;
        self.audio = None;

        if let Some(error) = combine_errors(
            combine_errors(encoder_failure, capture_failure),
            audio_failure,
        ) {
            self.writer = None;
            self.pending_input = None;
            self.closed = true;
            self.phase = AvRecordingPhase::Closed;
            self.stats.phase = AvRecordingPhase::Closed;
            let error = combine_errors(Some(error), self.finish_metadata(false).err())
                .expect("finalization failure");
            self.failure = Some(error.clone());
            return Err(error);
        }

        let Some(writer) = self.writer.take() else {
            let error = internal_error(
                "The recording has no container writer to finalize, so no artifact can be produced."
                    .to_string(),
            );
            self.closed = true;
            self.phase = AvRecordingPhase::Closed;
            self.stats.phase = AvRecordingPhase::Closed;
            let error = combine_errors(Some(error), self.finish_metadata(false).err())
                .expect("finalization failure");
            self.failure = Some(error.clone());
            return Err(error);
        };
        self.closed = true;
        self.phase = AvRecordingPhase::Closed;
        self.stats.phase = AvRecordingPhase::Closed;
        let finalize_started = Instant::now();
        let finalized = writer.finalize().and_then(|mut artifact| {
            if self.restart_budget.attempts == 0 {
                artifact.path = self.output.to_string_lossy().into_owned();
            }
            match self.session_index.as_mut() {
                Some(index) => crate::encoder_restart::finish_index(index, artifact),
                None => Ok(artifact),
            }
        });
        AvStageTiming::observe(
            &mut self.stats.timing.mux_finalize,
            finalize_started.elapsed(),
        );
        match finalized {
            Ok(artifact) => {
                if let Err(error) = self.finish_metadata(true) {
                    self.failure = Some(error.clone());
                    return Err(error);
                }
                self.stats.timing.stop_to_artifact_ns = self
                    .drain_started
                    .map(|started| started.elapsed().as_nanos());
                Ok(artifact)
            }
            Err(error) => {
                let error = combine_errors(Some(error), self.finish_metadata(false).err())
                    .expect("finalization failure");
                self.failure = Some(error.clone());
                Err(error)
            }
        }
    }

    /// Cancels startup without treating the encoder's clean undrained verdict as a failure.
    pub(crate) fn abort_preparation(&mut self) -> Result<(), RecorderError> {
        let encoder_failure = self
            .encoder
            .as_mut()
            .and_then(|encoder| crate::video_recording::stop_preparation_encoder(encoder).err());
        self.encoder = None;
        let remaining_failure = self.release().err();
        match combine_errors(encoder_failure, remaining_failure) {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Releases every resource without producing an artifact.
    ///
    /// The video encoder is stopped first, the capture session second and the audio pipeline
    /// third; the container writer is dropped without being finalized, so the partial file —
    /// if it was created at all — stays on disk explicitly incomplete and is never reported
    /// as a recording. Idempotent, and `Drop` runs it as the safety net.
    pub fn abort(&mut self) -> Result<(), RecorderError> {
        let outcome = self.release();
        if let Err(error) = &outcome {
            self.failure.get_or_insert_with(|| error.clone());
        }
        outcome
    }

    // --------------------------------------------------------------- internals

    /// One bounded recording-phase step.
    fn poll_paused(&mut self) -> Result<AvRecordingProgress, RecorderError> {
        let mut progress = AvRecordingProgress {
            phase: AvRecordingPhase::Recording,
            ..AvRecordingProgress::default()
        };
        if self.pending_input.is_some() && self.advance_pending_input()? {
            progress.frames_submitted = 1;
        }
        progress.packets_queued = self.collect_video_packets(POLL_PACKET_BUDGET)?;
        progress.packets_written = self.write_ready_packets(POLL_PACKET_BUDGET, true)?;
        // Both native producers remain open. PCM before the pause boundary is
        // admitted and encoded; later PCM is discarded by the common timeline.
        if self.pending_input.is_none() {
            progress.capture_frames = u32::from(self.read_capture_surface()?);
        }
        self.service_audio_sources()?;
        for _ in 0..POLL_AUDIO_BUDGET {
            let packets = self.poll_audio()?;
            if packets.is_empty() {
                break;
            }
            progress.packets_queued += packets.len() as u32;
            self.queue_audio_packets(packets)?;
        }
        Ok(progress)
    }

    fn poll_recording(&mut self) -> Result<AvRecordingProgress, RecorderError> {
        if !self.work_outstanding() {
            // Idle time is not time spent waiting for owned encoder work.
            self.last_progress = Instant::now();
        }
        let mut progress = AvRecordingProgress {
            phase: AvRecordingPhase::Recording,
            ..AvRecordingProgress::default()
        };
        // Delivery first: the interleave is the consumer of the encoders' bounded queues,
        // and a full queue blocks admission.
        progress.packets_written = self.write_ready_packets(POLL_PACKET_BUDGET, true)?;

        // Live audio next, so the Opus encoders keep pace with the capture worker. Every
        // source read is paired with one mixer quantum, so neither a native source queue nor
        // the mixer's accepted pending buffer can grow. A diagnostic run may hold this
        // stream back.
        let audio_choked = self.harness.audio_choked(self.recording_started.elapsed());
        if !audio_choked {
            self.service_audio_sources()?;
        }
        if !audio_choked {
            for _ in 0..POLL_AUDIO_BUDGET {
                let packets = self.poll_audio()?;
                if packets.is_empty() {
                    break;
                }
                progress.audio_quanta += 1;
                progress.packets_queued += packets.len() as u32;
                self.queue_audio_packets(packets)?;
            }
        }
        self.stats.audio_choked = audio_choked;

        // A diagnostic run may queue one deliberately stale packet here, once.
        if let Some(after) = self.harness.stale_packet_after
            && !self.stale_packet_injected
            && self.recording_started.elapsed() >= after
        {
            self.stale_packet_injected = true;
            let behind = self
                .harness
                .stale_packet_behind
                .as_nanos()
                .min(u128::from(u64::MAX)) as u64;
            self.inject_stale_video_packet(behind)?;
        }

        // Audio was serviced above; bound the video batch so neither source
        // can monopolize the owner. read_capture_surface offers each input now.
        let accepted_before = self.stats.accepted_frames;
        for _ in 0..frame_windows::MAX_PACKETS {
            if self.pending_input.is_some() && !self.advance_pending_input()? {
                break;
            }
            if !self.read_capture_surface()? {
                break;
            }
            progress.capture_frames += 1;
        }
        progress.frames_submitted = (self.stats.accepted_frames - accepted_before) as u32;
        // Every packet the encoder already completed joins the buffer in the same step, so
        // the delivery gate always sees the real video frontier instead of a queue the
        // encoder is still holding.
        progress.packets_queued += self.collect_video_packets(POLL_PACKET_BUDGET)?;
        // The new surface may have produced packets that are already eligible.
        progress.packets_written += self.write_ready_packets(POLL_PACKET_BUDGET, true)?;
        self.note_progress();
        self.check_progress()?;
        Ok(progress)
    }

    /// One bounded drain step: video end of stream, the audio tail, then every remaining
    /// packet in container order.
    fn poll_draining(&mut self) -> Result<AvRecordingProgress, RecorderError> {
        let mut progress = AvRecordingProgress {
            phase: AvRecordingPhase::Draining,
            ..AvRecordingProgress::default()
        };

        // 1. The video encoder's owned input and its end of stream. Admission is already
        //    closed, so the last frame really is the last frame.
        if self.pending_input.is_some() && self.advance_pending_input()? {
            progress.frames_submitted = 1;
        }
        if self.pending_input.is_none() && !self.eos_requested {
            if let Some(encoder) = self.encoder.as_mut() {
                let started = Instant::now();
                let outcome = encoder.begin_drain();
                AvStageTiming::observe(
                    &mut self.stats.timing.stop_encoder_drain,
                    started.elapsed(),
                );
                outcome?;
            }
            self.eos_requested = true;
        }
        progress.packets_queued += self.collect_video_packets(POLL_PACKET_BUDGET)?;

        // 2. The audio drain: every source's last block, the mixer's frozen boundary and the
        //    encoders' one-shot flush. Its final packets join the interleave buffer in
        //    container order; nothing is written yet, because the video encoder may still be
        //    holding a packet whose timestamp precedes them.
        if !self.audio_drained && self.step_audio_drain(POLL_AUDIO_DRAIN_BUDGET)? {
            self.audio_drained = true;
        }
        progress.packets_queued += self.collect_video_packets(POLL_PACKET_BUDGET)?;

        // 3. Both producers must have ended before the tail is closed: the encoder
        //    acknowledged its end of stream and the audio drain reported its one-shot flush.
        //    Only then is the pool of packet timestamps fixed, and only then can the rest be
        //    written in queued order without a still-buffering video encoder handing over
        //    another packet. Cross-track late delivery is valid even during live writing.
        if !self.drain_tail_written && self.production_ended() {
            progress.packets_written += self.write_ready_packets(POLL_PACKET_BUDGET, true)?;
            // Close tracks only after every buffered packet was written.
            if !self.interleave.is_empty() {
                self.note_progress();
                self.check_progress()?;
                return Ok(progress);
            }
            let tail_started = Instant::now();
            let finished_audio = self.finish_audio_tracks();
            AvStageTiming::observe(&mut self.stats.timing.stop_tail, tail_started.elapsed());
            finished_audio?;
            let tail = self
                .stats
                .stop_pts_ns
                .zip(self.stats.last_video_pts_ns)
                .map(|(stop, last)| {
                    stop.saturating_sub(last)
                        .max(nominal_duration(self.config.rate_num, self.config.rate_den).get())
                })
                .ok_or_else(|| {
                    internal_error("The Stop boundary has no last video timestamp.".to_string())
                })?;
            let tail_started = Instant::now();
            let finished_video = if self.replay_only {
                Ok(())
            } else {
                self.writer
                    .as_mut()
                    .ok_or_else(|| internal_error("Missing recording writer.".into()))?
                    .finish_video(tail)
            };
            AvStageTiming::observe(&mut self.stats.timing.stop_tail, tail_started.elapsed());
            finished_video?;
            self.drain_tail_written = true;
        }

        // 4. Anything the encoder handed over while the tail above was being closed. That
        //    window is one poll: the next poll applies the patch before it writes again.
        progress.packets_queued += self.collect_video_packets(POLL_PACKET_BUDGET)?;
        progress.packets_written += self.write_ready_packets(POLL_PACKET_BUDGET, true)?;

        if self.audio_drained && self.video_eos_drained() {
            self.refresh_container_stats();
            self.phase = AvRecordingPhase::Drained;
            self.stats.phase = AvRecordingPhase::Drained;
            self.stats.timing.stop_to_drained_ns = self
                .drain_started
                .map(|started| started.elapsed().as_nanos());
            return Ok(AvRecordingProgress {
                phase: AvRecordingPhase::Drained,
                ..progress
            });
        }
        self.note_progress();
        self.check_progress()?;
        Ok(progress)
    }

    /// True once neither producer can hand over another packet: the audio drain reported its
    /// one-shot flush and the video encoder acknowledged its end of stream.
    fn production_ended(&self) -> bool {
        self.audio_drained && self.eos_requested && self.video_encoder_drained
    }

    /// True once the video encoder was asked for its end of stream, acknowledged it, owns
    /// nothing else and the interleave buffer is empty.
    fn video_eos_drained(&self) -> bool {
        self.eos_requested
            && self.video_encoder_drained
            && self.pending_input.is_none()
            && self.interleave.is_empty()
            && self.drain_tail_written
    }

    /// Reads at most one quantum from the live audio pipeline and encodes it.
    fn poll_audio(&mut self) -> Result<Vec<EncodedPacket>, RecorderError> {
        let now = self.active_timestamp(self.clock.now());
        let Some(audio) = self.audio.as_mut() else {
            return Err(internal_error(
                "The recording has no audio pipeline to poll.".to_string(),
            ));
        };
        let started = Instant::now();
        let batch = audio.poll(now);
        AvStageTiming::observe(&mut self.stats.timing.audio_encode, started.elapsed());
        batch
    }

    /// Takes the PCM every open source has ready off its bounded native queue, pairing each
    /// read with one mixer quantum.
    ///
    /// A native source queue is bounded, so a step that only waits for the mixer would let it
    /// fill; the pairing is what keeps both the source queue and the mixer's own accepted
    /// pending buffer bounded. Nothing is dropped, re-timed or resampled.
    fn service_audio_sources(&mut self) -> Result<(), RecorderError> {
        for _ in 0..POLL_AUDIO_SOURCE_BUDGET {
            let Some(audio) = self.audio.as_mut() else {
                return Err(internal_error(
                    "The recording has no audio pipeline to read sources from.".to_string(),
                ));
            };
            let started = Instant::now();
            let source_poll = audio.poll_sources();
            AvStageTiming::observe(&mut self.stats.timing.audio_source_poll, started.elapsed());
            if source_poll? == 0 {
                break;
            }
            let packets = self.poll_audio()?;
            self.queue_audio_packets(packets)?;
        }
        Ok(())
    }

    /// One bounded step of the graceful audio drain. It returns once the encoders were
    /// flushed exactly once and every track carried its real trailing trim.
    ///
    /// The pipeline is reached through the fields it lives in rather than through `&mut
    /// self`, because the packets a drain step produces are queued into the same recording
    /// that owns the pipeline.
    fn step_audio_drain(&mut self, budget: usize) -> Result<bool, RecorderError> {
        #[cfg(test)]
        let native_audio = self.lifecycle_script.is_none();
        #[cfg(not(test))]
        let native_audio = true;
        // Service native queues before starting the drain timing, as on the live path.
        if native_audio && !self.drain_requested {
            self.service_audio_sources()?;
        }
        let started = Instant::now();
        let outcome = self.poll_audio_drain(budget);
        AvStageTiming::observe(&mut self.stats.timing.stop_audio_drain, started.elapsed());
        let report = outcome?;
        if let Some(pts) = self.stats.max_observed_pts_ns {
            self.observe_pts(pts);
        }
        if let Some(tracks) = &report {
            // The live writer waits for every declared track's first packet and refuses
            // empty tracks. A completed zero-input encoder can never open that gate.
            // Fail before gated delivery; the control owner aborts this sticky failure.
            if let Some(index) = tracks.iter().position(|track| track.input_frames == 0) {
                return Err(RecorderError {
                    code: ErrorCode::InvalidRequest,
                    message: format!(
                        "Declared audio track {index} finished with zero PCM frames. Every announced audio track must contain recorded audio, so this recording cannot be finalized."
                    ),
                    recovery: RecoveryStatus::Unknown,
                    action: "Discard the incomplete recording and record again after the selected audio sources deliver PCM.".to_string(),
                });
            }
            for (index, track) in tracks.iter().enumerate() {
                if let Some(slot) = self.stats.audio_trims.get_mut(index) {
                    *slot = track.discard_padding;
                }
                self.stats.audio_input_frames[index] = track.input_frames;
            }
            self.audio_trims_taken = report;
            return Ok(true);
        }
        Ok(false)
    }

    fn poll_audio_drain(
        &mut self,
        budget: usize,
    ) -> Result<Option<Vec<OpusTrackFinish>>, RecorderError> {
        #[cfg(test)]
        if let Some(script) = self.lifecycle_script.as_mut() {
            self.drain_requested = true;
            return drain_audio_polls(
                || {
                    Ok(script
                        .audio
                        .pop_front()
                        .unwrap_or(EncodedAudioDrainPoll::Pending))
                },
                &mut self.interleave,
                &mut self.stats,
                &mut self.audio_firsts,
                self.plan.len(),
                budget,
            );
        }
        let mut audio = self.audio.take();
        let outcome = drain_audio_step(
            &mut audio,
            &mut self.drain_requested,
            &mut self.interleave,
            &mut self.stats,
            &mut self.audio_firsts,
            self.plan.len(),
            budget,
        );
        self.audio = audio;
        outcome
    }

    /// Closes every declared audio track with its real trailing trim, after the last block
    /// of every track is in the file.
    fn finish_audio_tracks(&mut self) -> Result<(), RecorderError> {
        let Some(tracks) = self.audio_trims_taken.take() else {
            return Err(internal_error(
                "The audio drain reported no track metadata to close with.".to_string(),
            ));
        };
        for (index, track) in tracks.iter().enumerate() {
            let written = self.segment_audio_written[index] * 960;
            let expected =
                track.input_frames + u64::from(track.pre_skip) + u64::from(track.discard_padding);
            if written != expected {
                return Err(internal_error(format!(
                    "Audio track {index} wrote {written} coded frames but its encoder requires {expected}."
                )));
            }
            if !self.replay_only {
                self.writer
                    .as_mut()
                    .ok_or_else(|| internal_error("Missing recording writer.".into()))?
                    .finish_audio(index, track.discard_padding)?;
            }
        }
        Ok(())
    }

    /// Number of attempted segment restarts, including an exhausted preparation.
    pub fn encoder_restart_count(&self) -> u32 {
        self.restart_budget.attempts
    }
    pub fn session_manifest(&self) -> Result<Option<frame_mux::SessionManifest>, RecorderError> {
        self.session_index
            .as_ref()
            .map(frame_mux::RecordingSession::snapshot)
            .transpose()
    }

    /// During preparation, keep the existing PCM/mixer/Opus flow alive and write
    /// its packets only into the preceding segment. No cross-segment packet reuse.
    fn service_restart_audio(&mut self) -> Result<(), RecorderError> {
        self.flush_restart_packets()?;
        let packets = self.poll_audio()?;
        self.queue_audio_packets(packets)?;
        self.flush_restart_packets()
    }

    fn flush_restart_packets(&mut self) -> Result<(), RecorderError> {
        // The buffer is bounded. The failed video worker will publish no later packet.
        while let Some(queued) = self.interleave.pop() {
            self.deliver_packet(queued)?;
        }
        Ok(())
    }

    fn restart_encoder_segment(&mut self) -> Result<(), RecorderError> {
        let mut stage = "native_cleanup";
        let mut preparation_frames = 0;
        let outcome = (|| {
            let old = self
                .encoder
                .as_mut()
                .ok_or_else(|| internal_error("Missing failed encoder.".into()))?;
            let id = old.info().capability.id.clone();
            let reason = old
                .failure()
                .ok_or_else(|| internal_error("No published encoder failure.".into()))?
                .message;
            let owner_pending = u64::from(self.pending_input.is_some());
            self.session_index
                .as_mut()
                .ok_or_else(|| internal_error("Missing session index.".into()))?
                .record_interruption_reason(&crate::encoder_restart::interruption_reason(
                    &reason,
                    &id,
                    old.counters(),
                    owner_pending,
                    0,
                    self.stats.failed_submit_frames,
                    None,
                ))?;
            crate::encoder_restart::release_failed(old)?;
            let failure_counters = old.counters();
            self.session_index
                .as_mut()
                .expect("checked index")
                .record_interruption_reason(&crate::encoder_restart::interruption_reason(
                    &reason,
                    &id,
                    failure_counters,
                    owner_pending,
                    0,
                    self.stats.failed_submit_frames,
                    None,
                ))?;
            self.encoder = None;
            self.pending_input = None;
            self.stats.restart_discarded_owner_frames += owner_pending;
            stage = "old_segment_delivery";
            self.flush_restart_packets()?;
            stage = "fresh_capture_and_audio_service";
            let deadline = Instant::now() + PREPARATION_WATCHDOG;
            let mut first = loop {
                self.service_restart_audio()?;
                if let Some(frame) = self
                    .session
                    .as_mut()
                    .ok_or_else(|| internal_error("Missing capture session.".into()))?
                    .try_frame()?
                {
                    break frame;
                }
                if Instant::now() >= deadline {
                    return Err(preparation_error(
                        "No capture surface for the new segment.".into(),
                    ));
                }
                std::thread::sleep(PREPARATION_POLL);
            };
            first.timestamp = self.active_timestamp(first.timestamp);
            preparation_frames = 1;
            stage = "replacement_preparation";
            let (encoder, first, declined) = crate::encoder_restart::prepare(
                first,
                self.config,
                id.clone(),
                self.restart_budget.alternative_backend,
                || self.service_restart_audio(),
            )?;
            self.encoder = Some(encoder);
            if let Some(capture) = self.session.as_mut() {
                capture.set_replay_sampling(true);
            }
            if let Some(encoder) = self.encoder.as_mut() {
                encoder.set_replay_keyframes(true);
            }
            self.attach_pipeline_wake();
            let stream = self
                .encoder
                .as_ref()
                .expect("prepared encoder")
                .info()
                .clone();
            stage = "audio_segment_rotation";
            let audio = self
                .audio
                .as_mut()
                .ok_or_else(|| internal_error("Missing audio pipeline.".into()))?;
            let finish = audio.rotate_segment()?;
            let origin = audio.segment_origin();
            let tracks = self
                .plan
                .iter()
                .enumerate()
                .map(|(index, plan)| {
                    AvAudioTrack::from_plan(
                        plan,
                        audio
                            .pre_skip(index)
                            .ok_or_else(|| internal_error("Missing new Opus encoder.".into()))?,
                    )
                    .and_then(|track| track.with_channels(audio.track_channels(index)))
                })
                .collect::<Result<Vec<_>, _>>()?;
            self.queue_audio_packets(finish.packets)?;
            self.flush_restart_packets()?;
            self.audio_trims_taken = Some(finish.tracks);
            self.finish_audio_tracks()?;
            stage = "old_segment_checkpoint";
            if let Some(writer) = self.writer.as_mut()
                && writer.stats().video_packets > 0
            {
                writer.checkpoint()?;
            }
            self.writer = None; // Failed video never receives a successful stream finalization.
            stage = "replacement_manifest";
            let index = self
                .session_index
                .as_mut()
                .ok_or_else(|| internal_error("No session index for restart.".into()))?;
            index.record_interruption_reason(&crate::encoder_restart::interruption_reason(
                &reason,
                &id,
                failure_counters,
                owner_pending,
                0,
                self.stats.failed_submit_frames,
                Some((
                    &stream.capability.id,
                    declined.as_deref(),
                    u64::from(first.timestamp < origin),
                )),
            ))?;
            let writer = index.create_av_segment(&stream, &tracks, origin, true)?;
            stage = "replacement_replay_contract";
            self.replay.reset_stream(stream, tracks)?;
            self.writer = Some(writer);
            self.segment_origin = origin;
            if let Some(metadata) = &self.metadata {
                metadata.segment(origin)?;
            }
            // Native preparation may outlast the borrowed initial picture. Fresh audio
            // has a later sample boundary; never submit a video PTS before that origin.
            self.pending_input = if first.timestamp >= origin {
                Some(EncoderInput {
                    input: first,
                    duration_ns: nominal_duration(self.config.rate_num, self.config.rate_den),
                })
            } else {
                self.stats.restart_stale_preparation_frames += 1;
                None
            };
            self.previous_frame_pts = self
                .pending_input
                .as_ref()
                .map(|input| input.input.timestamp);
            self.video_first = false;
            self.audio_firsts.fill(false);
            self.segment_audio_written.fill(0);
            self.max_observed_pts_ns = None;
            self.last_progress = Instant::now();
            self.last_progress_marker = self.progress_marker();
            Ok(())
        })();
        outcome.map_err(|error| {
            crate::encoder_restart::record_restart_failure(
                self.session_index.as_mut(),
                stage,
                preparation_frames,
                error,
            )
        })
    }

    /// Takes every video packet the encoder is ready to hand over, up to `budget`.
    fn collect_video_packets(&mut self, budget: usize) -> Result<u32, RecorderError> {
        let started = Instant::now();
        let outcome = self.collect_video_packets_inner(budget);
        AvStageTiming::observe(&mut self.stats.timing.video_output, started.elapsed());
        outcome
    }

    fn collect_video_packets_inner(&mut self, budget: usize) -> Result<u32, RecorderError> {
        let mut queued = 0u32;
        for _ in 0..budget {
            #[cfg(test)]
            let scripted = self
                .lifecycle_script
                .as_mut()
                .map(|script| script.video.pop_front().unwrap_or(EncoderPoll::Pending));
            #[cfg(not(test))]
            let scripted: Option<EncoderPoll> = None;
            let poll = match (scripted, self.encoder.as_mut()) {
                (Some(poll), _) => poll,
                (None, Some(encoder)) => encoder.try_packet()?,
                (None, None) => {
                    return Err(internal_error(
                        "The recording has no encoder session to take packets from.".to_string(),
                    ));
                }
            };
            match poll {
                EncoderPoll::Packet(packet) => {
                    self.queue_video_packet(packet)?;
                    queued += 1;
                }
                EncoderPoll::Pending => break,
                EncoderPoll::Drained => {
                    // The encoder acknowledged its end of stream: it can never produce
                    // another packet, so the last one it produced is final.
                    self.video_encoder_drained = true;
                    break;
                }
            }
        }
        Ok(queued)
    }

    /// Records and queues one video packet.
    fn queue_video_packet(&mut self, packet: EncodedPacket) -> Result<(), RecorderError> {
        if self.restart_budget.attempts > 0 && !self.video_first {
            let stream = self
                .encoder
                .as_ref()
                .ok_or_else(|| internal_error("Missing replacement encoder.".into()))?
                .info();
            crate::encoder_restart::validate_first_packet(stream, &packet)?;
        }
        self.video_first = true;
        self.stats.video_packets += 1;
        self.stats.video_bytes += packet.data.len() as u64;
        if packet.keyframe {
            self.stats.video_keyframes += 1;
        }
        self.stats.first_video_pts_ns.get_or_insert(packet.pts.0);
        self.stats.last_video_pts_ns = Some(packet.pts.0);
        self.observe_pts(packet.pts.0);
        self.mark_ready();
        self.interleave.push(packet, PacketOrigin::Video)
    }

    /// Records and queues one batch of encoded audio packets.
    fn queue_audio_packets(&mut self, packets: Vec<EncodedPacket>) -> Result<(), RecorderError> {
        for packet in packets {
            let index = packet.track_id as usize;
            if index >= self.plan.len() {
                return Err(internal_error(format!(
                    "An encoded audio packet carries track id {}, which is not one of the {} planned tracks.",
                    packet.track_id,
                    self.plan.len()
                )));
            }
            self.audio_firsts[index] = true;
            self.stats.audio_packets += 1;
            self.stats.audio_bytes += packet.data.len() as u64;
            self.stats.first_audio_pts_ns.get_or_insert(packet.pts.0);
            self.stats.last_audio_pts_ns = Some(packet.pts.0);
            self.observe_pts(packet.pts.0);
            self.mark_ready();
            self.interleave.push(packet, PacketOrigin::Audio(index))?;
        }
        Ok(())
    }

    /// Writes every packet that is allowed to be written now, in container order.
    ///
    /// While recording, a packet is written only once the slowest announced stream is
    /// [`DELIVERY_DELAY`] past it. This bounds the queue's ordinary latency; it is
    /// not a promise that another track cannot later deliver an earlier PTS. During
    /// the drain nothing new can arrive, so everything left is written at once. `allow_audio`
    /// is `false` only while the video tail still has to be closed: the live duration patch
    /// is valid only while the last video block is still the last block written.
    fn write_ready_packets(
        &mut self,
        budget: usize,
        allow_audio: bool,
    ) -> Result<u32, RecorderError> {
        let started = Instant::now();
        let outcome = self.write_ready_packets_inner(budget, allow_audio);
        AvStageTiming::observe(&mut self.stats.timing.mux_write, started.elapsed());
        outcome
    }

    fn write_ready_packets_inner(
        &mut self,
        budget: usize,
        allow_audio: bool,
    ) -> Result<u32, RecorderError> {
        // Nothing is written before every announced track delivered its first packet.
        if !self.has_all_firsts() {
            return Ok(0);
        }
        let mut written = 0u32;
        while (written as usize) < budget {
            let Some(earliest) = self.interleave.earliest_pts_ns() else {
                break;
            };
            let Some(next_origin) = self.interleave.earliest_origin() else {
                break;
            };
            if !allow_audio && matches!(next_origin, PacketOrigin::Audio(_)) {
                break;
            }
            if let Some(gate) = self.delivery_gate_ns()
                && earliest > gate
            {
                self.stats.delayed_deliveries += 1;
                break;
            }
            let Some(queued) = self.interleave.pop() else {
                break;
            };
            self.deliver_packet(queued)?;
            written += 1;
        }
        if written > 0 {
            self.refresh_container_stats();
        }
        Ok(written)
    }

    fn deliver_packet(&mut self, queued: QueuedPacket) -> Result<(), RecorderError> {
        match queued.origin {
            PacketOrigin::Video => {
                self.replay.push_video(&queued.packet);
                if !self.replay_only {
                    self.writer
                        .as_mut()
                        .ok_or_else(|| internal_error("Missing recording writer.".into()))?
                        .write_video(queued.packet)?;
                }
                self.stats.video_packets_written += 1;
            }
            PacketOrigin::Audio(index) => {
                self.replay.push_audio(&queued.packet);
                if !self.replay_only {
                    self.writer
                        .as_mut()
                        .ok_or_else(|| internal_error("Missing recording writer.".into()))?
                        .write_audio(queued.packet)?;
                }
                self.stats.audio_packets_written += 1;
                self.stats.audio_written_by_track[index] += 1;
                self.segment_audio_written[index] += 1;
                // Audio delivery does not alter video timing.
                if let Some(writer) = self.writer.as_ref() {
                    self.stats.video_duration_patches = writer.stats().video_duration_patches;
                }
            }
        }
        Ok(())
    }

    /// True once every announced track delivered its first packet.
    fn has_all_firsts(&self) -> bool {
        self.video_first && self.audio_firsts.iter().all(|first| *first)
    }

    /// Offers the pending input once. `true` means the encoder accepted it.
    fn advance_pending_input(&mut self) -> Result<bool, RecorderError> {
        let Some(input) = self.pending_input.take() else {
            return Ok(true);
        };
        let started = Instant::now();
        let outcome = match self.encoder.as_mut() {
            Some(encoder) => encoder.submit(input),
            None => {
                return Err(internal_error(
                    "The recording has no encoder session to submit a frame to.".to_string(),
                ));
            }
        };
        AvStageTiming::observe(&mut self.stats.timing.video_submit, started.elapsed());
        let outcome = outcome.inspect_err(|_| {
            self.stats.failed_submit_frames += 1;
        })?;
        match outcome {
            SubmitOutcome::Accepted => {
                self.stats.accepted_frames += 1;
                Ok(true)
            }
            SubmitOutcome::Backpressure(input) => {
                self.stats.backpressure_returns += 1;
                self.pending_input = Some(input);
                Ok(false)
            }
        }
    }

    /// Reads at most one completed capture surface and submits it immediately.
    ///
    /// A new surface is never held back for a lookahead: the encoder receives it in the same
    /// step, and its duration is the interval the source measured to it — the explicit
    /// nominal period only for the very first frame, which has no predecessor.
    fn read_capture_surface(&mut self) -> Result<bool, RecorderError> {
        let mut frame = {
            let Some(session) = self.session.as_mut() else {
                return Err(internal_error(
                    "The recording has no capture session to read from.".to_string(),
                ));
            };
            let started = Instant::now();
            session.begin_frames()?;
            let frame = session.try_frame();
            AvStageTiming::observe(&mut self.stats.timing.video_capture, started.elapsed());
            match frame? {
                // No ready source update is not a failure: a static desktop keeps its live
                // session and this recording simply owns no new work.
                None => return Ok(false),
                Some(frame) => frame,
            }
        };
        let timeline = self
            .pause_timeline
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if timeline
            .pause_boundary_raw()
            .is_some_and(|boundary| frame.timestamp.0 >= boundary.0)
        {
            return Ok(true);
        }
        if timeline
            .resume_after()
            .is_some_and(|boundary| frame.timestamp.0 < boundary.0)
        {
            return Ok(true);
        }
        frame.timestamp = timeline
            .map(frame.timestamp)
            .ok_or_else(|| invalid_request_error("Capture pause mapping underflowed.".into()))?;
        drop(timeline);
        if frame.timestamp < self.segment_origin {
            return Ok(true);
        }
        self.stats.capture_frames += 1;
        if self.admission.admit(frame.timestamp)? == FrameAdmission::SkipByPolicy {
            return Ok(true);
        }
        let duration = match self.previous_frame_pts {
            Some(previous) => {
                let Some(interval) = frame
                    .timestamp
                    .0
                    .checked_sub(previous.0)
                    .filter(|ns| *ns > 0)
                else {
                    return Err(invalid_request_error(format!(
                        "Capture surface timestamps must increase strictly, but {} ns followed {} ns.",
                        frame.timestamp.0, previous.0
                    )));
                };
                self.stats.last_video_interval_ns = Some(interval);
                NonZeroU64::new(interval).expect("the interval was checked to be non-zero")
            }
            None => {
                self.stats
                    .first_video_pts_ns
                    .get_or_insert(frame.timestamp.0);
                nominal_duration(self.config.rate_num, self.config.rate_den)
            }
        };
        self.previous_frame_pts = Some(frame.timestamp);
        self.pending_input = Some(EncoderInput {
            input: frame,
            duration_ns: duration,
        });
        // The new input is offered in this same poll, so encoding follows the surface
        // immediately instead of waiting for another frame.
        self.advance_pending_input()?;
        Ok(true)
    }

    /// Records where a packet sits on the shared timeline.
    ///
    /// The furthest timestamp any accepted packet reached is the delivery gate's anchor: a
    /// packet is eligible once the gate has moved past it. This does not bound
    /// cross-track delivery lateness from a capture or encode worker.
    fn observe_pts(&mut self, pts_ns: u64) {
        self.max_observed_pts_ns = Some(
            self.max_observed_pts_ns
                .map_or(pts_ns, |previous| previous.max(pts_ns)),
        );
    }

    /// The ordinary delivery threshold: [`DELIVERY_DELAY`] behind the furthest
    /// timestamp any accepted packet reached. `None` until a packet is observed.
    ///
    /// The same bound applies while recording and while draining. It is only lifted once
    /// both producers ended ([`AvRecording::production_ended`]), because then no packet can
    /// arrive and the rest of the buffer is written in its queued order.
    fn delivery_gate_ns(&self) -> Option<u64> {
        if self.production_ended() {
            return None;
        }
        let max = self.max_observed_pts_ns?;
        let delay_ns = DELIVERY_DELAY.as_nanos().min(u128::from(u64::MAX)) as u64;
        Some(max.saturating_sub(delay_ns))
    }

    /// Records the one moment every announced track had delivered its first packet.
    fn mark_ready(&mut self) {
        if self.stats.ready || !self.has_all_firsts() {
            return;
        }
        self.stats.ready = true;
        self.stats.readiness_ms = self.recording_started.elapsed().as_millis();
    }

    /// Copies the writer's own counters so a caller can report container evidence.
    fn refresh_container_stats(&mut self) {
        if let Some(writer) = self.writer.as_ref() {
            let stats = writer.stats();
            self.stats.retained_bytes_estimate = stats.retained_bytes_estimate;
            self.stats.video_duration_patches = stats.video_duration_patches;
        }
    }

    fn refresh_stats(&mut self) {
        self.stats.phase = self.phase;
        self.stats.peak_pending_packets = self.interleave.peak_packets;
        self.stats.peak_pending_bytes = self.interleave.peak_bytes;
        self.stats.max_observed_pts_ns = self.max_observed_pts_ns;
        if let Some(drained) = self.stats.timing.stop_to_drained_ns {
            self.stats.drain_ms = drained / 1_000_000;
        } else if let Some(started) = self.drain_started {
            self.stats.drain_ms = started.elapsed().as_millis();
        }
    }

    /// A composite marker of delivered work. It only ever moves forward through real
    /// deliveries, so a missing capture surface cannot look like progress.
    fn progress_marker(&self) -> u64 {
        let counters = self
            .encoder
            .as_ref()
            .map(WindowsH264Encoder::counters)
            .unwrap_or_default();
        self.stats
            .video_packets_written
            .saturating_add(self.stats.audio_packets_written)
            .saturating_add(counters.accepted_inputs)
            .saturating_add(counters.native_submissions)
            .saturating_add(counters.native_buffered)
            .saturating_add(counters.packets_emitted)
            .saturating_add(counters.eos_acknowledgements)
    }

    fn note_progress(&mut self) {
        let marker = self.progress_marker();
        if marker != self.last_progress_marker {
            self.last_progress_marker = marker;
            self.last_progress = Instant::now();
        }
    }

    /// True while this recording owns work whose completion it is waiting for.
    ///
    /// Everything this session still holds counts: a pending input, an accepted input still
    /// inside the encoder, an outstanding end of stream, an unflushed interleave buffer or an
    /// unfinished audio drain. A session that simply has no new capture surface owns no work.
    fn work_outstanding(&self) -> bool {
        if self.pending_input.is_some() || !self.interleave.is_empty() {
            return true;
        }
        // An outstanding video end of stream is only work while the encoder has not
        // acknowledged it yet.
        if self.eos_requested && !self.video_encoder_drained {
            return true;
        }
        if self.phase == AvRecordingPhase::Draining && !self.audio_drained {
            return true;
        }
        let counters = match self.encoder.as_ref() {
            Some(encoder) => encoder.counters(),
            None => return false,
        };
        counters.queued_inputs > 0 || counters.submitted_in_flight() > 0
    }

    /// The explicit progress bound: work is expected, so silence is reported, never waited
    /// out. A native call that never returns cannot be forcibly cancelled.
    fn check_progress(&self) -> Result<(), RecorderError> {
        if !self.work_outstanding() {
            return Ok(());
        }
        let elapsed = self.last_progress.elapsed();
        if elapsed <= PROGRESS_WATCHDOG {
            return Ok(());
        }
        let counters = match self.encoder.as_ref() {
            Some(encoder) => encoder.counters(),
            None => Default::default(),
        };
        Err(stalled_error(format!(
            "The recording made no observable progress for {} ms while it still owned work: {} input(s) queued, {} native submission(s) in flight, {} packet(s) held by the interleave buffer ({} byte(s)), {} video and {} audio packet(s) written, {} accepted input(s), audio_drained={}. The last native call was: {}. A native call inside the encoder or capture worker cannot be forcibly cancelled, so the recording is reported as stalled instead of being waited out.",
            elapsed.as_millis(),
            counters.queued_inputs,
            counters.submitted_in_flight(),
            self.interleave.len(),
            self.interleave.bytes(),
            self.stats.video_packets_written,
            self.stats.audio_packets_written,
            counters.accepted_inputs,
            self.audio_drained,
            self.encoder
                .as_ref()
                .map(WindowsH264Encoder::native_call_phase)
                .unwrap_or_else(|| "no encoder session".to_string())
        )))
    }

    /// Releases the video encoder, the capture session, the audio pipeline and the container
    /// writer, in that order, without finalizing anything. Idempotent.
    fn release(&mut self) -> Result<(), RecorderError> {
        let metadata_failure = self
            .metadata
            .take()
            .and_then(|mut metadata| metadata.finish(false).err());
        let encoder_failure = match self.encoder.as_mut() {
            Some(encoder) => encoder.stop().err(),
            None => None,
        };
        let capture_failure = match self.session.as_mut() {
            Some(session) => session.stop().err(),
            None => None,
        };
        let audio_failure = match self.audio.as_mut() {
            Some(audio) => audio.stop().err(),
            None => None,
        };
        self.encoder = None;
        self.session = None;
        self.audio = None;
        // A partial container is retained for explicit recovery, never published as success.
        self.writer = None; // Preserve a recoverable prefix without returning an artifact.
        self.pending_input = None;
        self.interleave = Interleave::default();
        self.eos_requested = false;
        self.closed = true;
        self.phase = AvRecordingPhase::Closed;
        self.stats.phase = AvRecordingPhase::Closed;
        match combine_errors(
            combine_errors(encoder_failure, capture_failure),
            combine_errors(audio_failure, metadata_failure),
        ) {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl Drop for AvRecording {
    fn drop(&mut self) {
        // Safety net only: joins the native workers on this control thread and drops the
        // unfinalized container. It never detaches a worker and never produces an artifact.
        let _ = self.release();
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use frame_audio::TrackKind;
    use frame_contracts::AudioSourceId;
    use frame_encode::audio_pipeline::EncodedAudioFinish;
    use frame_encode::{BitstreamFormat, EncodedStreamInfo, EncoderCapability};
    use frame_mux::replay::{ReplayBuffer, ReplayLimit};
    use std::collections::VecDeque;
    use std::num::NonZeroU32;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    pub(super) struct ProducerScript {
        pub audio: VecDeque<EncodedAudioDrainPoll>,
        pub video: VecDeque<EncoderPoll>,
    }

    #[derive(Default)]
    struct CaptureWitness {
        stops: AtomicUsize,
        drops: AtomicUsize,
    }

    struct DeviceFreeCapture(Arc<CaptureWitness>);

    impl CaptureSession for DeviceFreeCapture {
        type Surface = WindowsSurface;

        fn try_frame(&mut self) -> Result<Option<CapturedFrame<WindowsSurface>>, RecorderError> {
            panic!("Stop must not read a capture surface");
        }

        fn stop(&mut self) -> Result<(), RecorderError> {
            self.0.stops.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(())
        }
    }

    impl Drop for DeviceFreeCapture {
        fn drop(&mut self) {
            self.0.drops.fetch_add(1, AtomicOrdering::SeqCst);
        }
    }

    fn packet(video: bool, track: u32, pts: u64) -> EncodedPacket {
        EncodedPacket {
            track_id: track,
            pts: MediaTimestamp(pts),
            dts: MediaTimestamp(pts),
            duration_ns: 20_000_000,
            keyframe: true,
            data: if video {
                vec![0, 0, 0, 1, 0x65, 0x11, 0x22, 0x33]
            } else {
                vec![0xFC, 0x12, 0x34]
            },
        }
    }

    // Script the accepted encoder completion contract: zero PCM emits no packet;
    // a 200-frame short tail emits one 960-frame packet with its real trim equation.
    fn completion(input_frames: &[u64], emit_packets: bool) -> EncodedAudioDrainPoll {
        let packets = input_frames
            .iter()
            .enumerate()
            .filter(|(_, frames)| **frames > 0 && emit_packets)
            .map(|(index, _)| packet(false, index as u32, 0))
            .collect();
        let tracks = input_frames
            .iter()
            .map(|frames| OpusTrackFinish {
                packets: Vec::new(),
                pre_skip: 312,
                discard_padding: if *frames == 0 {
                    0
                } else {
                    (960 - 312 - frames) as u16
                },
                input_frames: *frames,
            })
            .collect();
        EncodedAudioDrainPoll::Finished(EncodedAudioFinish { packets, tracks })
    }

    fn owner(
        name: &str,
        audio: Vec<EncodedAudioDrainPoll>,
        track_count: usize,
    ) -> (AvRecording, Arc<CaptureWitness>) {
        let stream = EncodedStreamInfo {
            capability: EncoderCapability {
                id: "scripted".into(),
                codec: "h264".into(),
                hardware: false,
            },
            width: NonZeroU32::new(64).unwrap(),
            height: NonZeroU32::new(48).unwrap(),
            nominal_rate_num: NonZeroU32::new(30).unwrap(),
            nominal_rate_den: NonZeroU32::new(1).unwrap(),
            bitstream_format: BitstreamFormat::H264AnnexB,
            colour: None,
            codec_config: vec![
                0, 0, 0, 1, 0x67, 0x64, 0, 0x3C, 0xAC, 0x2B, 0x40, 0x14, 0, 0, 0, 1, 0x68, 0xEE,
                0x3C, 0xB0,
            ],
            allows_reordering: false,
        };
        let plan: Vec<_> = (0..track_count)
            .map(|index| TrackPlan {
                kind: if index == 0 {
                    TrackKind::MainMix
                } else {
                    TrackKind::Clean(AudioSourceId(format!("source-{index}")))
                },
                sources: vec![AudioSourceId(format!("source-{index}"))],
            })
            .collect();
        let tracks: Vec<_> = plan
            .iter()
            .map(|track| AvAudioTrack::from_plan(track, 312).unwrap())
            .collect();
        let directory = std::env::var_os("FIX_001_EVIDENCE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("freym-runtime-fix-001"));
        assert!(directory.is_absolute());
        std::fs::create_dir_all(&directory).unwrap();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let output = directory.join(format!("{name}-{nonce}.mkv"));
        let mut session_index =
            frame_mux::RecordingSession::create(&crate::encoder_restart::manifest_path(&output))
                .unwrap();
        let writer = session_index
            .create_av_segment(&stream, &tracks, MediaTimestamp(0), true)
            .unwrap();
        println!("device-free fixture: {}", output.display());
        let witness = Arc::new(CaptureWitness::default());
        let now = Instant::now();
        let recording = AvRecording {
            replay: ReplayBuffer::new(
                stream,
                tracks,
                ReplayLimit {
                    duration_ns: 1_000_000_000,
                    max_bytes: 1024 * 1024,
                },
            )
            .unwrap(),
            disk_replay: None,
            replay_only: false,
            pause_timeline: Arc::new(Mutex::new(PauseTimeline::default())),
            paused: false,
            display_id: DisplayId("scripted".into()),
            config: NativeEncoderConfig::proof(
                64,
                48,
                NonZeroU32::new(30).unwrap(),
                NonZeroU32::new(1).unwrap(),
            )
            .unwrap(),
            plan,
            clock: Arc::new(MasterClock::default()),
            origin: MediaTimestamp(0),
            output,
            session: Some(Box::new(DeviceFreeCapture(witness.clone()))),
            admission: FrameAdmitter::auto(),
            encoder: None,
            audio: None,
            writer: Some(writer),
            session_index: Some(session_index),
            metadata: None,
            metadata_target: CaptureTarget::Display(DisplayId("scripted".into())),
            metadata_cursor_baked: false,
            restart_budget: crate::encoder_restart::RestartBudget::default(),
            segment_audio_written: vec![0; track_count],
            segment_origin: MediaTimestamp(0),
            phase: AvRecordingPhase::Recording,
            stop_requested: false,
            eos_requested: false,
            drain_requested: false,
            audio_drained: false,
            video_encoder_drained: false,
            audio_trims_taken: None,
            drain_tail_written: false,
            closed: false,
            failure: None,
            stats: AvRecordingStats {
                audio_trims: vec![0; track_count],
                audio_input_frames: vec![0; track_count],
                audio_written_by_track: vec![0; track_count],
                ..AvRecordingStats::default()
            },
            pending_input: None,
            previous_frame_pts: None,
            harness: Harness::default(),
            stale_packet_injected: false,
            interleave: Interleave::new(),
            audio_firsts: vec![false; track_count],
            video_first: false,
            max_observed_pts_ns: None,
            recording_started: now,
            drain_started: None,
            last_progress: now,
            last_progress_marker: 0,
            lifecycle_script: Some(ProducerScript {
                audio: audio.into(),
                video: vec![
                    EncoderPoll::Packet(packet(true, 0, 0)),
                    EncoderPoll::Drained,
                ]
                .into(),
            }),
        };
        (recording, witness)
    }

    fn assert_released(recording: &AvRecording, witness: &CaptureWitness) {
        assert_eq!(recording.phase, AvRecordingPhase::Closed);
        assert!(recording.closed);
        assert!(
            recording.session.is_none()
                && recording.encoder.is_none()
                && recording.audio.is_none()
                && recording.writer.is_none()
        );
        assert!(recording.pending_input.is_none() && recording.interleave.is_empty());
        assert_eq!(witness.stops.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(witness.drops.load(AtomicOrdering::SeqCst), 1);
    }

    #[test]
    fn zero_pcm_and_one_video_packet_fail_and_release() {
        let (mut recording, witness) = owner("zero-pcm", vec![completion(&[0], true)], 1);
        recording.request_stop().unwrap();
        let outcome = recording.poll();
        assert_eq!(recording.stats.video_packets, 1);
        assert_eq!(recording.stats.audio_packets, 0);
        let error = outcome
            .expect_err("zero PCM must terminate instead of returning Draining indefinitely");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(error.message.contains("track 0") && error.message.contains("zero PCM"));
        assert_eq!(recording.stats.video_packets_written, 0);
        assert_eq!(
            recording.poll().unwrap_err(),
            error,
            "failure stays terminal"
        );
        assert_eq!(
            recording.finish().unwrap_err(),
            error,
            "no artifact on failure"
        );
        assert_released(&recording, &witness);
        recording.abort().unwrap();
        assert_released(&recording, &witness);
    }

    #[test]
    fn one_empty_declared_track_is_not_silently_dropped() {
        let (mut recording, witness) =
            owner("empty-second-track", vec![completion(&[200, 0], true)], 2);
        recording.request_stop().unwrap();
        let error = recording
            .poll()
            .expect_err("every declared track must contain PCM");
        assert!(error.message.contains("track 1") && error.message.contains("zero PCM"));
        assert_eq!(recording.stats.audio_packets, 1);
        assert_eq!(recording.stats.video_packets_written, 0);
        assert_eq!(recording.finish().unwrap_err(), error);
        assert_released(&recording, &witness);
    }

    #[test]
    fn replay_only_audio_tail_drains_without_a_growing_recording_writer() {
        let (mut recording, witness) = owner(
            "replay-only-tail",
            vec![EncodedAudioDrainPoll::Pending, completion(&[200], true)],
            1,
        );
        recording.enable_replay_only().unwrap();
        recording.request_stop().unwrap();
        assert_eq!(recording.poll().unwrap().phase, AvRecordingPhase::Draining);
        assert_eq!(recording.poll().unwrap().phase, AvRecordingPhase::Drained);
        assert_eq!(recording.stats.audio_packets_written, 1);
        assert_eq!(recording.stats.video_packets_written, 1);
        assert!(recording.writer.is_none() && recording.session_index.is_none());
        recording.abort().unwrap();
        assert_released(&recording, &witness);
    }

    #[test]
    fn short_audio_tail_drains_and_finalizes() {
        let (mut recording, witness) = owner(
            "short-tail",
            vec![EncodedAudioDrainPoll::Pending, completion(&[200], true)],
            1,
        );
        recording.request_stop().unwrap();
        assert_eq!(recording.poll().unwrap().phase, AvRecordingPhase::Draining);
        assert_eq!(recording.poll().unwrap().phase, AvRecordingPhase::Drained);
        assert_eq!(recording.stats.audio_input_frames, [200]);
        assert_eq!(recording.stats.audio_trims, [448]);
        assert_eq!(recording.stats.audio_packets_written, 1);
        assert_eq!(recording.stats.video_packets_written, 1);
        let artifact = recording
            .finish()
            .expect("the real live writer finalizes the short tail");
        assert_eq!(artifact.path, recording.output.to_string_lossy());
        assert!(std::fs::metadata(&recording.output).unwrap().len() > 0);
        let manifest = recording.session_manifest().unwrap().unwrap();
        assert!(manifest.completed);
        assert_eq!(manifest.segments[0].state, frame_mux::SegmentState::Closed);
        assert_eq!(
            manifest.segments[0].file_bytes,
            std::fs::metadata(&recording.output).unwrap().len()
        );
        assert_released(&recording, &witness);
    }

    #[test]
    fn stalled_nonempty_interleave_reaches_watchdog_without_resetting_progress() {
        // Fault injection: a completed nonempty track never delivers its first packet.
        // This must reach the watchdog even after the empty-PCM policy is applied.
        let (mut recording, witness) =
            owner("stalled-interleave", vec![completion(&[200], false)], 1);
        recording.request_stop().unwrap();
        recording.poll().unwrap();
        assert!(recording.production_ended() && !recording.interleave.is_empty());
        let aged = Instant::now() - PROGRESS_WATCHDOG - Duration::from_millis(1);
        recording.last_progress = aged;
        recording.last_progress_marker = recording.progress_marker();
        let error = recording
            .poll()
            .expect_err("a held interleave must not bypass the watchdog");
        assert_eq!(
            recording.last_progress, aged,
            "a zero-work poll is not progress"
        );
        assert!(error.message.contains("no observable progress"));
        assert_eq!(recording.finish().unwrap_err(), error);
        assert_released(&recording, &witness);
    }

    #[test]
    fn pending_audio_drain_also_reaches_watchdog() {
        let (mut recording, witness) =
            owner("pending-audio", vec![EncodedAudioDrainPoll::Pending], 1);
        recording.request_stop().unwrap();
        recording.poll().unwrap();
        recording.last_progress = Instant::now() - PROGRESS_WATCHDOG - Duration::from_millis(1);
        recording.last_progress_marker = recording.progress_marker();
        let error = recording.poll().expect_err("pending audio remains bounded");
        assert!(error.message.contains("no observable progress"));
        assert_eq!(recording.finish().unwrap_err(), error);
        assert_released(&recording, &witness);
    }
}

// ------------------------------------------------------------------- helpers

/// One bounded audio drain step over the pipeline field it drains.
///
/// It returns the one-shot flush metadata once the encoders were flushed exactly once, with
/// every final packet already in the interleave buffer in container order. Nothing is
/// dropped or re-timed, and no track is closed here: the caller writes the final packets and
/// only then patches each track's trailing trim.
fn drain_audio_step(
    audio: &mut Option<EncodedAudioPipeline<NormalizedAudio>>,
    drain_requested: &mut bool,
    interleave: &mut Interleave,
    stats: &mut AvRecordingStats,
    audio_firsts: &mut [bool],
    plan_len: usize,
    budget: usize,
) -> Result<Option<Vec<OpusTrackFinish>>, RecorderError> {
    let Some(pipeline) = audio.as_mut() else {
        return Err(internal_error(
            "The recording has no audio pipeline to drain.".to_string(),
        ));
    };
    if !*drain_requested {
        pipeline.request_drain()?;
        *drain_requested = true;
    }
    drain_audio_polls(
        || pipeline.poll_drain(),
        interleave,
        stats,
        audio_firsts,
        plan_len,
        budget,
    )
}

/// Shared drain delivery path for native and device-free producer outputs.
fn drain_audio_polls(
    mut poll: impl FnMut() -> Result<EncodedAudioDrainPoll, RecorderError>,
    interleave: &mut Interleave,
    stats: &mut AvRecordingStats,
    audio_firsts: &mut [bool],
    plan_len: usize,
    budget: usize,
) -> Result<Option<Vec<OpusTrackFinish>>, RecorderError> {
    for _ in 0..budget {
        let step = poll()?;
        match step {
            EncodedAudioDrainPoll::Packets(batch) => {
                // A live drain quantum is encoded exactly like a live one: it joins the
                // same interleave buffer in container order and is written with it.
                queue_drained_audio(batch, interleave, stats, audio_firsts, plan_len)?;
            }
            // Neither a contended lock nor a source that is still draining is a verdict.
            EncodedAudioDrainPoll::Pending => return Ok(None),
            EncodedAudioDrainPoll::Drained => {
                // A plan always has at least one encoder, so `Drained` before the one-shot
                // finish report would mean a track was never trimmed.
                return Err(internal_error(
                    "The audio pipeline reported a completed drain before its encoders were flushed."
                        .to_string(),
                ));
            }
            EncodedAudioDrainPoll::Finished(report) => {
                // The final packets join the buffer; the caller writes them and then closes
                // every track with the codec's own trailing trim.
                queue_drained_audio(report.packets, interleave, stats, audio_firsts, plan_len)?;
                return Ok(Some(report.tracks));
            }
        }
    }
    Ok(None)
}

/// Adds one drained audio batch to the interleave buffer, recording each packet exactly
/// like a live one: its track's first-packet flag, its byte and packet counters and its
/// position on the shared timeline.
fn queue_drained_audio(
    packets: Vec<EncodedPacket>,
    interleave: &mut Interleave,
    stats: &mut AvRecordingStats,
    audio_firsts: &mut [bool],
    plan_len: usize,
) -> Result<(), RecorderError> {
    for packet in packets {
        let index = packet.track_id as usize;
        if index >= plan_len {
            return Err(internal_error(format!(
                "An encoded audio packet carries track id {}, which is not one of the {plan_len} planned tracks.",
                packet.track_id
            )));
        }
        if let Some(first) = audio_firsts.get_mut(index) {
            *first = true;
        }
        stats.audio_packets += 1;
        stats.audio_bytes += packet.data.len() as u64;
        stats.first_audio_pts_ns.get_or_insert(packet.pts.0);
        stats.last_audio_pts_ns = Some(packet.pts.0);
        stats.max_observed_pts_ns = Some(
            stats
                .max_observed_pts_ns
                .map_or(packet.pts.0, |previous| previous.max(packet.pts.0)),
        );
        interleave.push(packet, PacketOrigin::Audio(index))?;
    }
    Ok(())
}

/// Waits for the first completed capture surface within the preparation budget.
///
/// Preparation is also the first place audio can arrive, so every live quantum the audio
/// pipeline has ready is taken here and returned to the caller, which queues it once the
/// container exists. Nothing is left in a native queue to overflow while the display
/// session is still starting, and nothing is dropped.
fn wait_for_first_surface(
    session: &mut dyn CaptureSession<Surface = WindowsSurface>,
    audio: &mut EncodedAudioPipeline<NormalizedAudio>,
    clock: &MasterClock,
    cancel_start: &AtomicBool,
) -> Result<(CapturedFrame<WindowsSurface>, Vec<EncodedPacket>), RecorderError> {
    let deadline = Instant::now() + PREPARATION_WATCHDOG;
    let mut packets = Vec::new();
    loop {
        crate::recording_control::check_preparation_cancelled(cancel_start)?;
        if let Some(frame) = session.try_frame()? {
            return Ok((frame, packets));
        }
        for _ in 0..POLL_AUDIO_BUDGET {
            crate::recording_control::check_preparation_cancelled(cancel_start)?;
            let batch = audio.poll(clock.now())?;
            if batch.is_empty() {
                break;
            }
            let bytes = packets
                .iter()
                .map(|p: &EncodedPacket| p.data.len())
                .sum::<usize>()
                + batch.iter().map(|p| p.data.len()).sum::<usize>();
            if packets.len() + batch.len() > MAX_INTERLEAVE_PACKETS.max(audio.tracks().len() * 32)
                || bytes as u64 > MAX_INTERLEAVE_BYTES
            {
                return Err(invalid_request_error(
                    "Preparation exceeded the interleave packet or byte budget.".to_string(),
                ));
            }
            packets.extend(batch);
        }
        if Instant::now() >= deadline {
            return Err(preparation_error(format!(
                "The capture session delivered no completed surface within {} ms, so the recording was not prepared.",
                PREPARATION_WATCHDOG.as_millis()
            )));
        }
        std::thread::sleep(PREPARATION_POLL);
    }
}

#[cfg(test)]
mod preparation_tests {
    use super::*;

    fn fixture_output(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join("freym-av-preparation-fix-002");
        std::fs::create_dir_all(&directory).unwrap();
        directory.join(format!(
            "{name}-{}.mkv",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    use frame_audio::{AudioBackend, AudioCaptureBackend, AudioContext, AudioSession, PcmBlock};
    use frame_contracts::{AudioSource, AudioSourceId};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Witness {
        opens: AtomicUsize,
        stops: AtomicUsize,
        drops: AtomicUsize,
    }
    struct Session(Arc<Witness>);
    impl AudioSession for Session {
        fn try_pcm(&mut self) -> Result<Option<PcmBlock>, RecorderError> {
            panic!("preparation cancellation must not poll PCM");
        }
        fn stop(&mut self) -> Result<(), RecorderError> {
            self.0.stops.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    impl Drop for Session {
        fn drop(&mut self) {
            self.0.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct Backend<'a> {
        witness: Arc<Witness>,
        cancel: &'a AtomicBool,
        cancel_on_open: usize,
    }
    impl AudioBackend for Backend<'_> {
        fn enumerate_audio_sources(&self) -> Result<Vec<AudioSource>, RecorderError> {
            panic!("scripted preparation must not discover devices");
        }
    }
    impl AudioCaptureBackend for Backend<'_> {
        type Session = Session;
        fn start_audio(
            &mut self,
            _: &AudioSourceId,
            _: AudioContext,
        ) -> Result<Session, RecorderError> {
            let number = self.witness.opens.fetch_add(1, Ordering::SeqCst) + 1;
            if number == self.cancel_on_open {
                self.cancel.store(true, Ordering::Release);
            }
            Ok(Session(self.witness.clone()))
        }
    }

    #[test]
    fn fix002_each_audio_open_is_guarded_and_partial_sources_are_released() {
        let cancel = AtomicBool::new(false);
        let witness = Arc::new(Witness::default());
        let mut backend = PreparationAudioBackend {
            backend: Backend {
                witness: witness.clone(),
                cancel: &cancel,
                cancel_on_open: 2,
            },
            cancel: &cancel,
        };
        let routes: Vec<_> = ["c", "a", "b"]
            .into_iter()
            .map(|id| AudioRouteSelection {
                controls: Default::default(),
                muted: false,
                source_id: AudioSourceId(id.into()),
                route: AudioRoute::MainMix,
            })
            .collect();
        let error = AudioPipeline::start(
            &mut backend,
            &routes,
            Arc::new(MasterClock::default()),
            MediaTimestamp(0),
        )
        .err()
        .unwrap();
        assert_eq!(error.code, ErrorCode::InvalidTransition);
        assert_eq!(witness.opens.load(Ordering::SeqCst), 2);
        assert_eq!(witness.stops.load(Ordering::SeqCst), 2);
        assert_eq!(witness.drops.load(Ordering::SeqCst), 2);
        assert!(
            backend
                .start_audio(
                    &AudioSourceId("d".into()),
                    AudioContext {
                        clock: Arc::new(MasterClock::default()),
                        cancellation: frame_audio::AudioCancellation::default(),
                    }
                )
                .is_err()
        );
        assert_eq!(witness.opens.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn fix002_cancelled_av_entry_never_reaches_capture_or_output() {
        let output = fixture_output("never-av");
        let request = AvRecordingRequest {
            display_id: DisplayId("scripted".into()),
            window_id: None,
            config: NativeEncoderConfig::proof(
                64,
                48,
                std::num::NonZeroU32::new(30).unwrap(),
                std::num::NonZeroU32::new(1).unwrap(),
            )
            .unwrap(),
            encoder_id: None,
            routes: vec![],
            plan: vec![],
            clock: Arc::new(MasterClock::default()),
            origin: MediaTimestamp(0),
            output: output.clone(),
            meter: Arc::new(MeterHub::default()),
            meter_session: "scripted".into(),
            camera: None,
            show_cursor: true,
            hdr_main10: false,
        };
        let error = AvRecording::start_with_preview_cancellable(
            request,
            None,
            FrameAdmitter::auto(),
            None,
            &AtomicBool::new(true),
        )
        .err()
        .unwrap();
        assert_eq!(error.code, ErrorCode::InvalidTransition);
        assert!(!output.exists());
    }
}
