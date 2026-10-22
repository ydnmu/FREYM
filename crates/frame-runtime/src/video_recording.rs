//! Reusable video-only recording owner for the desktop host (FRAME-0110A-VIDEO).
//!
//! One [`VideoRecording`] binds three accepted native pieces to one another:
//!
//! - the DXGI display capture session ([`WindowsCaptureSession`]) opened for one
//!   explicit [`DisplayId`] with [`FpsMode::AutoMatchSource`], so the captured surfaces
//!   and their media timestamps come from the session's own clock;
//! - the hardware encoder opened on the capture device's own pool and geometry;
//!   display capture prepares its surfaces before activating live DXGI acquisition;
//! - the single-video Matroska writer ([`MkvVideoWriter`]) created from the encoder's
//!   own stream description and the caller's shared origin, so every packet keeps its
//!   real position on the shared timeline.
//!
//! The capture and encoder workers own media. A bounded durability worker owns
//! periodic file sync and manifest publication, without retaining frames or packets.
//! One control thread drives this owner through
//! [`VideoRecording::start`], a bounded [`VideoRecording::poll`],
//! [`VideoRecording::request_stop`] and [`VideoRecording::finish`].
//!
//! This owner does not handle audio capture, mixing or UI. Each recording has an
//! atomic segment index. Opt-in encoder recovery permits one same-config restart
//! after proven native cleanup, on a fresh capture surface and independent MKV track.
//! That transition joins the failed owner and uses a temporary preparation thread.
//!
//! Frame policy
//! ------------
//!
//! - **No invented frame and no invented timestamp.** The first surface the capture
//!   session delivers is this session's first input, and its timestamp is the session
//!   timestamp: it is never rebased to zero. The writer maps each packet as
//!   `packet.pts - origin` for the shared origin the caller supplied.
//! - **Variable frame intervals keep their exact source time** through exactly one
//!   lookahead frame: a frame's duration is `duration_until(pts, next_pts)`, and the
//!   last frame — which has no later source timestamp — receives the explicit nominal
//!   period ([`nominal_duration`]).
//! - **Admission pressure is preserved, never dropped.** Backpressure returns the exact
//!   owned input, which stays this session's pending input, and the pending input is
//!   advanced *before* any new capture surface is read.
//!
//! Bounds
//! ------
//!
//! - preparation waits at most [`PREPARATION_WATCHDOG`] for the first completed capture
//!   surface;
//! - one `poll` writes at most the encoder's own bounded packet queue
//!   ([`frame_windows::MAX_PACKETS`]) to the writer, advances a bounded input batch
//!   and reads a batch bounded by `MAX_PACKETS`;
//! - the bounded [`PROGRESS_WATCHDOG`] applies only while this recording still owns
//!   work: a pending input, an accepted input or an outstanding end of stream. A static
//!   desktop that simply delivers no new surface is *not* a failure and is never turned
//!   into one, and no `poll` sleeps or spins waiting for a frame.
//!
//! Teardown
//! --------
//!
//! `request_stop` closes admission immediately and permanently. After the pending input
//! and the last frame have been delivered, the order is: encoder end of stream, every
//! produced packet, encoder stop, capture stop, container finalize. The encoder is
//! stopped before the capture session in every path — including a failure — because the
//! measured native behaviour on the shared D3D11 device is that stopping capture while
//! a hardware encoder session is still encoding can leave the capture worker inside its
//! copy/query path for an unbounded time (the FRAME-0104 evidence; the same order the
//! `encode_probe` proof uses).
//!
//! A failure is sticky, and releasing the resources is owned by [`VideoRecording::abort`]
//! (also run by `Drop`), which stops the encoder, stops the capture session and drops
//! the container writer *without* finalizing it. No artifact is ever produced before a
//! successful, completed drain. Nothing here detaches a worker, calls `process::exit` or
//! claims that a native join can be forcibly interrupted: teardown joins exactly the
//! workers the native implementations own, and a native call that never returns cannot
//! be cancelled.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use frame_capture::{CaptureCancellation, CaptureContext, CaptureSession, CapturedFrame};
use frame_clock::{MasterClock, MediaTimestamp, PauseTimeline};
use frame_contracts::{
    CaptureTarget, DisplayId, ErrorCode, FpsMode, RecorderError, RecordingArtifact, RecoveryStatus,
};
use frame_encode::admission::{FrameAdmission, FrameAdmitter};
use frame_encode::{
    EncoderInput, EncoderPoll, PacketEncoder, SubmitOutcome, duration_until, nominal_duration,
};
use frame_mux::{ContainerWriter, MkvVideoWriter};
use frame_windows::{
    NativeEncoderConfig, NativeEncoderSettings, WindowsH264Encoder, WindowsSurface,
};

/// The backend publishes this exact verdict only after clean early shutdown/join.
/// Preparation never drains/finalizes; all other native/cleanup errors remain failures.
pub(crate) fn preparation_encoder_result(
    result: Result<(), RecorderError>,
) -> Result<(), RecorderError> {
    match result {
        Err(error)
            if error.code == ErrorCode::FinalizationFailed
                && error.message == "The encoder stopped without completing the stream." =>
        {
            Ok(())
        }
        other => other,
    }
}

pub(crate) fn stop_preparation_encoder(
    encoder: &mut WindowsH264Encoder,
) -> Result<(), RecorderError> {
    preparation_encoder_result(encoder.stop())
}

/// The bounded wall-clock budget for the first completed capture surface.
///
/// A display that never delivers a completed surface is reported instead of waited out;
/// preparation is the one place where this owner waits for native work.
pub const PREPARATION_WATCHDOG: Duration = Duration::from_secs(5);

/// The bounded wall-clock budget between two observable steps while this recording
/// still owns work (a pending input, an accepted-but-unfinished input or an outstanding
/// end of stream).
///
/// It never applies to a live session that merely produced no new surface: an idle
/// desktop with no pending input owns no work and is never failed for staying idle. A
/// native call inside either worker cannot be forcibly cancelled, so a real stall is
/// reported instead of waited out.
pub const PROGRESS_WATCHDOG: Duration = Duration::from_secs(5);

/// How many packets one `poll` may hand to the container writer.
///
/// This is the encoder's own bounded delivery queue ([`frame_windows::MAX_PACKETS`]), so
/// a poll can never loop for an unbounded time even while the encoder keeps producing.
/// The remaining packets are written by the following polls.
const POLL_PACKET_BUDGET: usize = frame_windows::MAX_PACKETS;

/// How long preparation waits between two readiness checks of the capture session.
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
        action: "Verify the display is active and try again.".to_string(),
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
        action: "Do not treat this recording as complete; report the failure.".to_string(),
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

/// Reject a frozen codec/source mismatch before opening capture or audio devices.
/// Main10 encoder profiles and HDR capture are distinct; this owner currently
/// supplies either BGRA8 SDR or scRGB HDR to its selected profile.
pub(crate) fn validate_encoder_signal(
    hdr_main10: bool,
    encoder_id: Option<&str>,
) -> Result<(), RecorderError> {
    if let Some(id) = encoder_id {
        let main10 = id.starts_with("nvidia-nvenc-hevc-main10-d3d11:")
            || id.starts_with("nvidia-nvenc-av1-main10-d3d11:");
        if main10 != hdr_main10 {
            return Err(invalid_request_error(format!(
                "Selected encoder {id} does not match the {} capture signal.",
                if hdr_main10 {
                    "HDR Main10"
                } else {
                    "8-bit SDR"
                }
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- public model

/// Everything one video-only recording needs before it starts.
///
/// The request is explicit by construction: the display identity, the encoder
/// configuration, the shared session clock, the shared origin and a *new* output path
/// are all supplied by the caller. The configuration must describe the display's native
/// resolution; a mismatch is rejected instead of being silently re-scaled.
pub struct VideoRecordingRequest {
    /// The display to capture. The session captures this display as a whole with
    /// [`FpsMode::AutoMatchSource`].
    pub display_id: DisplayId,
    /// Exact selected WGC window; when present no display capture is opened.
    pub window_id: Option<frame_contracts::WindowId>,
    /// The hardware encoder configuration, at the display's native resolution.
    pub config: NativeEncoderConfig,
    /// Frozen policy selection, checked against the first real capture texture.
    pub encoder_id: Option<String>,
    /// The one shared session clock injected into the capture session.
    pub clock: Arc<MasterClock>,
    /// The shared session origin the container subtracts from every packet timestamp.
    pub origin: MediaTimestamp,
    /// The output container path. It must not exist: a recording never overwrites one.
    pub output: PathBuf,
    pub camera: Option<frame_contracts::CameraSelection>,
    pub show_cursor: bool,
    /// Frozen capability-selected FP16 scRGB -> NVENC HEVC Main10 path.
    pub hdr_main10: bool,
}

impl std::fmt::Debug for VideoRecordingRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VideoRecordingRequest")
            .field("display_id", &self.display_id)
            .field("config", &self.config)
            .field("origin", &self.origin)
            .field("output", &self.output)
            .finish_non_exhaustive()
    }
}

/// The lifecycle phase of one recording.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VideoRecordingPhase {
    /// Admission is open: new capture surfaces are still read and submitted.
    #[default]
    Recording,
    /// `request_stop` closed admission; the pending input and the last frame are still
    /// being delivered and the end of stream is still in progress.
    Draining,
    /// The encoder acknowledged end of stream and every produced packet was written.
    /// [`VideoRecording::finish`] now produces the artifact.
    Drained,
    /// Every resource was released. A successful `finish` produced the artifact; an
    /// `abort`, a failed `finish` or `Drop` released the same resources without one.
    Closed,
}

impl VideoRecordingPhase {
    fn describe(self) -> &'static str {
        match self {
            Self::Recording => "recording",
            Self::Draining => "draining",
            Self::Drained => "drained",
            Self::Closed => "closed",
        }
    }
}

/// What one bounded [`VideoRecording::poll`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VideoRecordingProgress {
    pub phase: VideoRecordingPhase,
    /// Capture surfaces consumed in one bounded batch.
    pub capture_frames: u32,
    /// Inputs handed to the encoder, bounded by MAX_PACKETS.
    pub frames_submitted: u32,
    /// Packets this poll handed to the container writer, bounded by the encoder's own
    /// delivery queue.
    pub packets_written: u32,
}

/// One snapshot of what this recording delivered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VideoRecordingStats {
    pub phase: VideoRecordingPhase,
    /// Completed capture surfaces this session consumed.
    pub capture_frames: u64,
    /// Inputs the encoder accepted. Never a fabricated or duplicated frame.
    pub accepted_frames: u64,
    /// Owned inputs the encoder returned under pressure; each was retried unchanged.
    pub backpressure_returns: u64,
    /// Unaccepted owner-held surfaces explicitly discarded at a failed segment.
    pub restart_discarded_owner_frames: u64,
    /// Submission errors consume an unaccepted input; report them explicitly.
    pub failed_submit_frames: u64,
    /// Packets written to the container.
    pub packets_written: u64,
    /// Annex-B payload bytes written to the container.
    pub packet_bytes: u64,
    pub keyframes_written: u64,
    /// First and last source timestamps handed to the encoder, unmodified.
    pub first_packet_pts_ns: Option<u64>,
    pub last_packet_pts_ns: Option<u64>,
    /// First container timestamp, with the shared origin already subtracted.
    pub first_container_pts_ns: Option<u64>,
    /// End of the last written packet on the container timeline.
    pub last_container_end_ns: u64,
    /// Everything the writer still retained at the last snapshot.
    pub retained_bytes_estimate: u64,
    /// Wall-clock time from `start` to readiness.
    pub preparation_ms: u128,
    /// Wall-clock time from `request_stop` to now, while draining.
    pub draining_ms: u128,
}

/// Which packets one bounded delivery step produced.
#[derive(Clone, Copy, Debug, Default)]
struct Delivered {
    written: u32,
    drained: bool,
}

/// One running video-only recording.
///
/// It owns the capture session, the encoder session and the container writer, and it is
/// driven by exactly one control thread. `poll` bounds native work but writes synchronously; `finish`
/// joins the two native workers on the order the shared D3D11 device requires.
pub struct VideoRecording {
    replay: frame_mux::replay::ReplayBuffer,
    disk_replay: Option<frame_mux::replay::RollingReplayStore>,
    replay_only: bool,
    clock: Arc<MasterClock>,
    pause_timeline: Arc<Mutex<PauseTimeline>>,
    paused: bool,
    display_id: DisplayId,
    config: NativeEncoderConfig,
    origin: MediaTimestamp,
    output: PathBuf,
    session: Option<Box<dyn CaptureSession<Surface = WindowsSurface>>>,
    admission: FrameAdmitter,
    encoder: Option<WindowsH264Encoder>,
    writer: Option<MkvVideoWriter>,
    session_index: Option<frame_mux::RecordingSession>,
    metadata: Option<crate::metadata::RecordingMetadata>,
    metadata_target: CaptureTarget,
    metadata_cursor_baked: bool,
    restart_budget: crate::encoder_restart::RestartBudget,
    /// The one frame kept back so the previous frame's duration is the real interval to
    /// the next source timestamp. It is never dropped while the session records.
    lookahead: Option<CapturedFrame<WindowsSurface>>,
    /// The one owned input the encoder has not accepted yet. Under backpressure this is
    /// the *same* input that was offered, never a replacement.
    pending: Option<EncoderInput<CapturedFrame<WindowsSurface>>>,
    phase: VideoRecordingPhase,
    stop_requested: bool,
    eos_requested: bool,
    closed: bool,
    failure: Option<RecorderError>,
    stats: VideoRecordingStats,
    drain_started: Option<Instant>,
    /// When this recording last observed progress while it owned work, and the composite
    /// marker that decided it. Nothing is inferred from the absence of new frames alone.
    last_progress: Instant,
    last_progress_marker: u64,
}

impl std::fmt::Debug for VideoRecording {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VideoRecording")
            .field("display_id", &self.display_id)
            .field("output", &self.output)
            .field("phase", &self.phase)
            .field("closed", &self.closed)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl VideoRecording {
    /// Prepares one recording and returns it ready to accept frames.
    ///
    /// Preparation is bounded: full display capture allocates its existing pool,
    /// registers encoder surfaces and creates the container before live acquisition.
    /// Backends needing real input wait at most [`PREPARATION_WATCHDOG`] for their
    /// first surface and keep it for submission. Every actual frame retains its
    /// original timestamp. Failure cleanup closes the encoder before capture.
    pub fn start(request: VideoRecordingRequest) -> Result<Self, RecorderError> {
        Self::start_with_selection(request, None, FrameAdmitter::auto())
    }

    /// Starts the same owner with a canonical region and a frozen frame admission policy.
    pub fn start_with_selection(
        request: VideoRecordingRequest,
        region: Option<frame_contracts::Region>,
        admission: FrameAdmitter,
    ) -> Result<Self, RecorderError> {
        Self::start_with_preview(request, region, admission, None)
    }

    /// Uses this recording's display capture session for an optional bounded preview.
    pub fn start_with_preview(
        request: VideoRecordingRequest,
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
        request: VideoRecordingRequest,
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
        let restart_budget = crate::encoder_restart::RestartBudget::from_environment()?;
        validate_encoder_signal(request.hdr_main10, request.encoder_id.as_deref())?;
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
        let metadata_target = if let Some(id) = &request.window_id {
            CaptureTarget::Window(id.clone())
        } else if let Some(region) = &region {
            CaptureTarget::Region(region.clone())
        } else {
            CaptureTarget::Display(request.display_id.clone())
        };
        let started = Instant::now();
        checkpoint!();
        let context = CaptureContext {
            clock: Arc::clone(&request.clock),
            cancellation: CaptureCancellation::default(),
        };
        let mut session: Box<dyn CaptureSession<Surface = WindowsSurface>> =
            if let Some(window_id) = &request.window_id {
                Box::new(
                    frame_windows::WindowsWindowCaptureSession::start_selected_with_preview_cursor(
                        window_id,
                        FpsMode::AutoMatchSource,
                        context,
                        preview.cloned(),
                        request.show_cursor,
                    )?,
                )
            } else {
                match region {
                    Some(region) => {
                        let capture =
                            if let Some(hub) = preview.filter(|_| request.camera.is_none()) {
                                frame_windows::RegionCaptureSession::start_with_preview_cursor(
                                    &region,
                                    context,
                                    Arc::clone(hub),
                                    request.show_cursor,
                                )?
                            } else {
                                frame_windows::RegionCaptureSession::start_with_cursor(
                                    &region,
                                    context,
                                    request.show_cursor,
                                )?
                            };
                        Box::new(capture)
                    }
                    None => {
                        let target = CaptureTarget::Display(request.display_id.clone());
                        let capture = if request.camera.is_none() {
                            frame_windows::WindowsCaptureSession::prepare_recording_display(
                                &request.display_id,
                                context,
                                request.show_cursor,
                                request.hdr_main10,
                                preview.cloned(),
                            )?
                        } else {
                            frame_windows::start_display_with_cursor(
                                &target,
                                FpsMode::AutoMatchSource,
                                context,
                                request.show_cursor,
                            )?
                        };
                        Box::new(capture)
                    }
                }
            };

        checkpoint!(session.stop());
        if let Some(choice) = &request.camera {
            session = Box::new(frame_windows::compositor::CameraCaptureSession::start(
                session,
                choice,
                Arc::clone(&request.clock),
                request.config.width,
                request.config.height,
                preview.cloned(),
            )?);
        }

        let preparation_surface = match session.preparation_surface() {
            Ok(surface) => surface,
            Err(error) => {
                return Err(
                    combine_errors(Some(error), session.stop().err()).expect("primary error")
                );
            }
        };
        // Immediate backends retain their actual first image. A prepared backend
        // supplies only device/format ownership; no recording image exists yet.
        let first = if preparation_surface.is_some() {
            None
        } else {
            match wait_for_first_surface(session.as_mut(), cancel_start) {
                Ok(frame) => Some(frame),
                Err(error) => {
                    return Err(
                        combine_errors(Some(error), session.stop().err()).expect("primary error")
                    );
                }
            }
        };
        if let Some(first) = &first
            && let Err(error) = admission.admit(first.timestamp)
        {
            return Err(combine_errors(Some(error), session.stop().err()).expect("primary error"));
        }
        let surface = preparation_surface
            .as_ref()
            .or_else(|| first.as_ref().map(|frame| &frame.surface))
            .expect("prepared backing or actual first frame");

        checkpoint!(session.stop());
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
                return Err(
                    combine_errors(Some(error), session.stop().err()).expect("primary error")
                );
            }
        };
        // NVENC owns the backing/registrations, not a pool lease. Return the
        // preparation slot before acquisition; it has never held source pixels.
        drop(preparation_surface);
        checkpoint!(
            crate::video_recording::stop_preparation_encoder(&mut encoder),
            session.stop()
        );
        let replay = frame_mux::replay::ReplayBuffer::new(
            encoder.info().clone(),
            Vec::new(),
            frame_mux::replay::ReplayLimit {
                duration_ns: 30_000_000_000,
                max_bytes: 64 * 1024 * 1024,
            },
        )
        .map_err(|error| {
            let failure = combine_errors(Some(error), encoder.stop().err());
            combine_errors(failure, session.stop().err()).expect("primary error")
        })?;
        checkpoint!(
            crate::video_recording::stop_preparation_encoder(&mut encoder),
            session.stop()
        );
        let indexed_writer = (|| {
            let mut index = frame_mux::RecordingSession::create(
                &crate::encoder_restart::manifest_path(&request.output),
            )?;
            let writer = index.create_video_segment(encoder.info(), request.origin)?;
            Ok::<_, RecorderError>((index, writer))
        })();
        let (session_index, writer) = match indexed_writer {
            Ok(prepared) => prepared,
            Err(error) => {
                // Encoder first, capture second: the measured native order for the
                // shared D3D11 device. The encoder was never drained, so its own
                // stop reports an incomplete stream; the container failure is the
                // reported one.
                let error = combine_errors(Some(error), encoder.stop().err());
                return Err(combine_errors(error, session.stop().err()).expect("primary error"));
            }
        };

        let preparation_ms = started.elapsed().as_millis();
        let stats = VideoRecordingStats {
            phase: VideoRecordingPhase::Recording,
            capture_frames: u64::from(first.is_some()),
            preparation_ms,
            ..VideoRecordingStats::default()
        };
        let now = Instant::now();
        let mut recording = Self {
            replay,
            disk_replay: None,
            replay_only: false,
            clock: request.clock,
            pause_timeline: Arc::new(Mutex::new(PauseTimeline::default())),
            paused: false,
            display_id: request.display_id,
            config: request.config,
            origin: request.origin,
            output: request.output,
            session: Some(session),
            admission,
            encoder: Some(encoder),
            writer: Some(writer),
            session_index: Some(session_index),
            metadata: None,
            metadata_target: metadata_target.clone(),
            metadata_cursor_baked: request.show_cursor,
            restart_budget,
            // The first real surface is the session's first input *and* the first
            // lookahead frame: its duration is fixed by the next surface's timestamp.
            lookahead: first,
            pending: None,
            phase: VideoRecordingPhase::Recording,
            stop_requested: false,
            eos_requested: false,
            closed: false,
            failure: None,
            stats,
            drain_started: None,
            last_progress: now,
            last_progress_marker: 0,
        };
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
            || serde_json::json!({"media_ns": recording.clock.now().0, "audio": false}),
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
            || self.stats.packets_written != 0
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
    pub fn encoder_qp_statistics(&self) -> Option<frame_windows::NativeQpStatistics> {
        self.encoder.as_ref().and_then(|e| e.qp_statistics())
    }
    pub fn config(&self) -> NativeEncoderConfig {
        self.config
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
    ///
    /// `None` once the encoder has been released.
    pub fn encoder_settings(&self) -> Option<&NativeEncoderSettings> {
        self.encoder.as_ref().map(|encoder| encoder.settings())
    }

    /// One snapshot of what this recording delivered.
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

    /// Nonblocking counters of the encoder this owner actually drives.
    pub fn encoder_stats(&self) -> Option<frame_windows::EncoderSessionStats> {
        self.encoder.as_ref().map(WindowsH264Encoder::stats)
    }

    pub fn stats(&self) -> VideoRecordingStats {
        self.stats
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

    /// Internal opt-in API; default recordings retain only the existing memory replay.
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
        if self.phase != VideoRecordingPhase::Recording || self.paused {
            return Err(invalid_transition_error(
                "Video recording is not running.".into(),
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
        if self.phase != VideoRecordingPhase::Recording || !self.paused {
            return Err(invalid_transition_error(
                "Video recording is not paused.".into(),
            ));
        }
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
    /// On the ordinary path it never sleeps or blocks on a native call: each step writes
    /// at most the encoder's bounded delivery queue and advances a bounded input batch
    /// and reads a batch bounded by MAX_PACKETS. A missing surface is not an error — a
    /// static desktop stays a live session — while the absence of any progress *while
    /// work is owned* is reported once [`PROGRESS_WATCHDOG`] has passed.
    ///
    /// While [`VideoRecordingPhase::Recording`], the pending input is advanced before a
    /// new surface is read, so backpressure pauses acquisition instead of growing
    /// memory or dropping an owned frame. Once `request_stop` closed admission, the same
    /// call delivers the pending input and the last frame, requests end of stream and
    /// writes every remaining packet until it reports [`VideoRecordingPhase::Drained`].
    ///
    /// An opted-in encoder restart joins the failed worker and prepares a new encoder
    /// before returning. Native preparation retains its existing timeout/cleanup limits.
    ///
    /// A returned error is sticky and terminal: `finish` reports the same failure and
    /// releases the resources, and `abort` (or `Drop`) does the same without an artifact.
    pub fn poll(&mut self) -> Result<VideoRecordingProgress, RecorderError> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        if self.closed {
            return Err(invalid_transition_error(
                "This recording is closed: it was finished, aborted or dropped, so it can no longer produce media."
                    .to_string(),
            ));
        }
        let mut outcome = match self.phase {
            VideoRecordingPhase::Recording if self.paused => self.poll_paused(),
            VideoRecordingPhase::Recording => self.poll_recording(),
            VideoRecordingPhase::Draining => self.poll_draining(),
            VideoRecordingPhase::Drained => Ok(VideoRecordingProgress {
                phase: VideoRecordingPhase::Drained,
                ..VideoRecordingProgress::default()
            }),
            VideoRecordingPhase::Closed => Err(invalid_transition_error(
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
                    && self.phase == VideoRecordingPhase::Recording
                    && !self.paused
                    && !self.stop_requested,
                error,
                worker,
            ) {
                outcome = self
                    .restart_encoder_segment()
                    .map(|()| VideoRecordingProgress {
                        phase: self.phase,
                        ..VideoRecordingProgress::default()
                    });
            }
        }
        if outcome.is_ok()
            && self.phase == VideoRecordingPhase::Recording
            && !self.paused
            && let Some(store) = self.disk_replay.as_mut()
            && let Err(error) = store.checkpoint_if_due(&self.replay)
        {
            outcome = Err(error);
        }
        if let Err(error) = &outcome {
            // The failure is sticky, and the resources stay owned by this value so that
            // `finish`, `abort` or `Drop` releases every one of them on the control
            // thread. An opted-in restart may already have joined the failed encoder.
            self.failure.get_or_insert_with(|| error.clone());
        }
        self.stats.phase = self.phase;
        if let Some(drain_started) = self.drain_started {
            self.stats.draining_ms = drain_started.elapsed().as_millis();
        }
        outcome
    }

    /// Closes admission permanently. Idempotent and non-blocking.
    ///
    /// No capture surface is read after this call. The inputs this session already owns
    /// — the pending input and the one lookahead frame — are still delivered by `poll`,
    /// and only then does the encoder receive its end of stream.
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
        if let Some(metadata) = &mut self.metadata {
            metadata.stopping()?;
        }
        self.stop_requested = true;
        self.phase = VideoRecordingPhase::Draining;
        let now = Instant::now();
        self.drain_started = Some(now);
        // The drain budget is measured from this request, not from the last frame: an
        // idle desktop may have delivered nothing for minutes before stop.
        self.last_progress = now;
        self.last_progress_marker = self.progress_marker();
        Ok(())
    }

    /// Completes the recording and produces the one artifact.
    ///
    /// It requires a completed drain (`poll` reported [`VideoRecordingPhase::Drained`]):
    /// a stream that was not fully delivered is never presented as a finished one. The
    /// teardown order is encoder stop, capture stop, container finalize — the measured
    /// native order for the capture and encoder workers that share one D3D11 device. Any
    /// teardown failure releases every resource and returns *without* finalizing the
    /// container. After success the owner is closed; a second finish is an invalid transition.
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
            // A failed recording is never finalized. Releasing still happens here, and
            // `abort`/`Drop` would do the same.
            let _ = self.release();
            return Err(error);
        }
        if self.phase != VideoRecordingPhase::Drained {
            return Err(invalid_transition_error(format!(
                "The recording is {}: keep calling poll() until it reports drained, or abort it. A container is never finalized from a stream that was not fully delivered.",
                self.phase.describe()
            )));
        }
        self.refresh_container_stats();

        // 1. The encoder is stopped first: destroying the hardware session is what the
        //    shared-device evidence requires before the capture worker is torn down.
        let encoder_failure = match self.encoder.as_mut() {
            Some(encoder) => encoder.stop().err(),
            None => None,
        };
        // 2. Capture is stopped in every case, so a failed encoder teardown still
        //    releases the capture worker instead of leaving it behind.
        let capture_failure = match self.session.as_mut() {
            Some(session) => session.stop().err(),
            None => None,
        };
        self.encoder = None;
        self.session = None;

        if let Some(error) = combine_errors(encoder_failure, capture_failure) {
            // The session did not release cleanly, so nothing is reported as a
            // recording: the container is dropped without being finalized.
            self.writer = None;
            self.lookahead = None;
            self.pending = None;
            self.closed = true;
            self.phase = VideoRecordingPhase::Closed;
            self.stats.phase = VideoRecordingPhase::Closed;
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
            self.phase = VideoRecordingPhase::Closed;
            self.stats.phase = VideoRecordingPhase::Closed;
            let error = combine_errors(Some(error), self.finish_metadata(false).err())
                .expect("finalization failure");
            self.failure = Some(error.clone());
            return Err(error);
        };
        self.closed = true;
        self.phase = VideoRecordingPhase::Closed;
        self.stats.phase = VideoRecordingPhase::Closed;
        let finalized = writer.finalize().and_then(|mut artifact| {
            if self.restart_budget.attempts == 0 {
                artifact.path = self.output.to_string_lossy().into_owned();
            }
            match self.session_index.as_mut() {
                Some(index) => crate::encoder_restart::finish_index(index, artifact),
                None => Ok(artifact),
            }
        });
        match finalized {
            Ok(artifact) => {
                if let Err(error) = self.finish_metadata(true) {
                    self.failure = Some(error.clone());
                    return Err(error);
                }
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
    /// The encoder is stopped first and the capture session second; the container writer
    /// is dropped without being finalized, so the partial file — if it was created at all
    /// — stays on disk explicitly incomplete and is never reported as a recording. This
    /// is idempotent, and `Drop` runs it as the safety net, so a failure path can never
    /// leave a worker or a device behind.
    pub fn abort(&mut self) -> Result<(), RecorderError> {
        let outcome = self.release();
        if let Err(error) = &outcome {
            self.failure.get_or_insert_with(|| error.clone());
        }
        outcome
    }

    // --------------------------------------------------------------- internals

    pub fn encoder_restart_count(&self) -> u32 {
        self.restart_budget.attempts
    }
    pub fn session_manifest(&self) -> Result<Option<frame_mux::SessionManifest>, RecorderError> {
        self.session_index
            .as_ref()
            .map(frame_mux::RecordingSession::snapshot)
            .transpose()
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
            let owner_pending = u64::from(self.pending.is_some());
            let owner_lookahead = u64::from(self.lookahead.is_some());
            self.session_index
                .as_mut()
                .ok_or_else(|| internal_error("Missing session index.".into()))?
                .record_interruption_reason(&crate::encoder_restart::interruption_reason(
                    &reason,
                    &id,
                    old.counters(),
                    owner_pending,
                    owner_lookahead,
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
                    owner_lookahead,
                    self.stats.failed_submit_frames,
                    None,
                ))?;
            self.encoder = None;
            stage = "old_segment_checkpoint";
            if let Some(writer) = self.writer.as_mut()
                && writer.stats().packets > 0
            {
                writer.checkpoint()?;
            }
            // The old incomplete tail stays recoverable, never finalized as a full recording.
            self.writer = None;
            self.pending = None;
            self.lookahead = None;
            self.stats.restart_discarded_owner_frames += owner_pending + owner_lookahead;
            stage = "fresh_capture";
            let deadline = Instant::now() + PREPARATION_WATCHDOG;
            let mut first = loop {
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
                || Ok(()),
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
                owner_lookahead,
                self.stats.failed_submit_frames,
                Some((&stream.capability.id, declined.as_deref(), 0)),
            ))?;
            let writer = index.create_video_segment(&stream, first.timestamp)?;
            if let Some(metadata) = &self.metadata {
                metadata.segment(first.timestamp)?;
            }
            stage = "replacement_replay_contract";
            self.replay.reset_stream(stream, Vec::new())?;
            self.writer = Some(writer);
            self.lookahead = Some(first);
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

    /// One bounded recording-phase step: deliver, advance, then read at most one surface.
    fn poll_paused(&mut self) -> Result<VideoRecordingProgress, RecorderError> {
        let mut progress = VideoRecordingProgress {
            phase: VideoRecordingPhase::Recording,
            ..VideoRecordingProgress::default()
        };
        progress.packets_written = self.write_ready_packets(POLL_PACKET_BUDGET as u32)?.written;
        if self.pending.is_some() && self.advance_pending()? {
            progress.frames_submitted = 1;
        }
        if self.pending.is_none() {
            progress.capture_frames = u32::from(self.read_capture_surface()?);
        }
        Ok(progress)
    }

    fn poll_recording(&mut self) -> Result<VideoRecordingProgress, RecorderError> {
        // Idle time is not time spent waiting for owned encoder work. Arm the
        // watchdog before a new capture can turn into a pending input.
        if !self.work_outstanding() {
            self.last_progress = Instant::now();
        }
        let mut progress = VideoRecordingProgress {
            phase: VideoRecordingPhase::Recording,
            ..VideoRecordingProgress::default()
        };
        // The writer is the consumer of the encoder's bounded queue, and a full queue
        // blocks admission. Delivering first keeps the pipeline moving.
        let delivered = self.write_ready_packets(POLL_PACKET_BUDGET as u32)?;
        progress.packets_written = delivered.written;
        if delivered.drained {
            return Err(internal_error(
                "The encoder reported a completed drain before this recording was asked to stop."
                    .to_string(),
            ));
        }
        // Drain the bounded ready queue in one turn. Pressure preserves the
        // exact pending input, and a quiet source ends the batch immediately.
        for _ in 0..frame_windows::MAX_PACKETS {
            if self.pending.is_some() {
                if !self.advance_pending()? {
                    break;
                }
                progress.frames_submitted += 1;
            }
            if !self.read_capture_surface()? {
                break;
            }
            progress.capture_frames += 1;
        }
        if self.pending.is_some()
            && progress.frames_submitted < frame_windows::MAX_PACKETS as u32
            && self.advance_pending()?
        {
            progress.frames_submitted += 1;
        }
        self.note_progress();
        self.check_progress()?;
        Ok(progress)
    }

    /// One bounded drain step: deliver, finish the tail, request end of stream, deliver.
    fn poll_draining(&mut self) -> Result<VideoRecordingProgress, RecorderError> {
        let mut progress = VideoRecordingProgress {
            phase: VideoRecordingPhase::Draining,
            ..VideoRecordingProgress::default()
        };
        let delivered = self.write_ready_packets(POLL_PACKET_BUDGET as u32)?;
        progress.packets_written = delivered.written;
        if delivered.drained {
            // End of stream is only acknowledged after `begin_drain`, which this session
            // requests below once every owned input has been handed over. A completed
            // drain before that would mean an owned frame was left behind.
            if !self.eos_requested {
                return Err(internal_error(
                    "The encoder reported a completed drain before this recording requested end of stream, so an owned input would be dropped."
                        .to_string(),
                ));
            }
            return self.complete_drain(progress);
        }
        // The pending input is delivered before the last frame is queued, and the last
        // frame is queued before end of stream. Backpressure pauses the drain instead of
        // dropping the owned input; the next poll retries the same value.
        if self.pending.is_some() {
            if self.advance_pending()? {
                progress.frames_submitted = 1;
            } else {
                self.note_progress();
                self.check_progress()?;
                return Ok(progress);
            }
        }
        if self.pending.is_none()
            && progress.frames_submitted == 0
            && let Some(last) = self.lookahead.take()
        {
            // The last frame has no strictly later source timestamp, so it receives the
            // explicit nominal period instead of an invented one.
            self.pending = Some(EncoderInput {
                input: last,
                duration_ns: nominal_duration(self.config.rate_num, self.config.rate_den),
            });
            if self.advance_pending()? {
                progress.frames_submitted = 1;
            } else {
                self.note_progress();
                self.check_progress()?;
                return Ok(progress);
            }
        }
        // Every accepted input has been handed over, so the end of stream is requested.
        if self.pending.is_none() && self.lookahead.is_none() && !self.eos_requested {
            if let Some(encoder) = self.encoder.as_mut() {
                encoder.begin_drain()?;
            }
            self.eos_requested = true;
        }
        if self.eos_requested {
            // The same per-poll bound holds across both delivery steps of a drain poll.
            let remaining = POLL_PACKET_BUDGET as u32 - progress.packets_written;
            let delivered = self.write_ready_packets(remaining)?;
            progress.packets_written += delivered.written;
            if delivered.drained {
                return self.complete_drain(progress);
            }
        }
        self.note_progress();
        self.check_progress()?;
        Ok(progress)
    }

    /// Marks the drain complete once the encoder acknowledged it with an empty queue.
    fn complete_drain(
        &mut self,
        progress: VideoRecordingProgress,
    ) -> Result<VideoRecordingProgress, RecorderError> {
        self.refresh_container_stats();
        self.phase = VideoRecordingPhase::Drained;
        self.stats.phase = VideoRecordingPhase::Drained;
        Ok(VideoRecordingProgress {
            phase: VideoRecordingPhase::Drained,
            ..progress
        })
    }

    /// Writes at most `budget` ready packets to the container, in the encoder's order.
    fn write_ready_packets(&mut self, budget: u32) -> Result<Delivered, RecorderError> {
        let mut delivered = Delivered::default();
        while delivered.written < budget {
            let poll = match self.encoder.as_mut() {
                Some(encoder) => encoder.try_packet()?,
                None => {
                    return Err(internal_error(
                        "The recording has no encoder session to take packets from.".to_string(),
                    ));
                }
            };
            let packet = match poll {
                EncoderPoll::Packet(packet) => packet,
                EncoderPoll::Pending => break,
                EncoderPoll::Drained => {
                    delivered.drained = true;
                    break;
                }
            };
            let pts = packet.pts.0;
            let bytes = packet.data.len() as u64;
            let keyframe = packet.keyframe;
            if self.restart_budget.attempts > 0
                && self
                    .writer
                    .as_ref()
                    .is_some_and(|writer| writer.stats().packets == 0)
            {
                let stream = self
                    .encoder
                    .as_ref()
                    .ok_or_else(|| internal_error("Missing replacement encoder.".into()))?
                    .info();
                crate::encoder_restart::validate_first_packet(stream, &packet)?;
            }
            // The packet is handed to the accepted single-video writer unchanged: the
            // writer owns validation, the origin mapping and the container timestamps.
            self.replay.push_video(&packet);
            let mux_started =
                frame_windows::cursor_preview_diagnostics::enabled().then(Instant::now);
            frame_windows::cursor_preview_diagnostics::record(
                "video_mux_begin",
                None,
                || serde_json::json!({"pts_ns": pts, "bytes": bytes}),
            );
            if !self.replay_only {
                self.writer
                    .as_mut()
                    .ok_or_else(|| internal_error("Missing recording writer.".into()))?
                    .write(packet)?;
            }
            delivered.written += 1;
            self.stats.packets_written += 1;
            self.stats.packet_bytes += bytes;
            if keyframe {
                self.stats.keyframes_written += 1;
            }
            self.stats.first_packet_pts_ns.get_or_insert(pts);
            self.stats.last_packet_pts_ns = Some(pts);
            frame_windows::cursor_preview_diagnostics::record("video_mux", None, || {
                serde_json::json!({"pts_ns": pts, "bytes": bytes,
                    "elapsed_ns": mux_started.map(|started| started.elapsed().as_nanos()),
                    "checkpoint": self.writer.as_ref().and_then(|writer| writer.stats().last_checkpoint)
                        .map(|point| point.sequence)})
            });
        }
        if delivered.written > 0 {
            self.refresh_container_stats();
        }
        Ok(delivered)
    }

    /// Offers the pending input once. `true` means the encoder accepted it.
    ///
    /// On backpressure the *same* owned input becomes the pending input again.
    fn advance_pending(&mut self) -> Result<bool, RecorderError> {
        let Some(input) = self.pending.take() else {
            return Ok(true);
        };
        let outcome = match self.encoder.as_mut() {
            Some(encoder) => match encoder.submit(input) {
                Ok(outcome) => outcome,
                Err(error) => {
                    self.stats.failed_submit_frames += 1;
                    return Err(error);
                }
            },
            None => {
                return Err(internal_error(
                    "The recording has no encoder session to submit a frame to.".to_string(),
                ));
            }
        };
        match outcome {
            SubmitOutcome::Accepted => {
                self.stats.accepted_frames += 1;
                Ok(true)
            }
            SubmitOutcome::Backpressure(input) => {
                self.stats.backpressure_returns += 1;
                self.pending = Some(input);
                Ok(false)
            }
        }
    }

    /// Reads at most one completed capture surface and fixes the previous frame's
    /// duration from the new timestamp. No surface is read once admission is closed.
    fn read_capture_surface(&mut self) -> Result<bool, RecorderError> {
        let Some(session) = self.session.as_mut() else {
            return Err(internal_error(
                "The recording has no capture session to read from.".to_string(),
            ));
        };
        session.begin_frames()?;
        let Some(mut frame) = session.try_frame()? else {
            // No ready source update is not a failure: a static desktop keeps its live
            // session and this recording simply owns no new work.
            return Ok(false);
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
        self.stats.capture_frames += 1;
        let decision = self.admission.admit(frame.timestamp)?;
        frame_windows::cursor_preview_diagnostics::record(
            "video_admission",
            None,
            || serde_json::json!({"pts_ns": frame.timestamp.0, "decision": format!("{decision:?}")}),
        );
        if decision == FrameAdmission::SkipByPolicy {
            return Ok(true);
        }
        if let Some(previous) = self.lookahead.take() {
            // The real interval to the next source timestamp, with checked arithmetic:
            // a repeated or backwards observation is an error, never a manufactured
            // duplicate frame.
            let duration = duration_until(previous.timestamp, frame.timestamp)?;
            self.pending = Some(EncoderInput {
                input: previous,
                duration_ns: duration,
            });
        }
        self.lookahead = Some(frame);
        Ok(true)
    }

    /// Copies the writer's own counters so a caller can report container evidence.
    fn refresh_container_stats(&mut self) {
        if let Some(writer) = self.writer.as_ref() {
            let stats = writer.stats();
            self.stats.first_container_pts_ns = stats.first_pts_ns;
            self.stats.last_container_end_ns = stats.last_end_ns;
            self.stats.retained_bytes_estimate = stats.retained_bytes_estimate;
        }
    }

    /// A composite marker of delivered work. It only ever moves forward through real
    /// deliveries, so a missing capture surface cannot look like progress.
    fn progress_marker(&self) -> u64 {
        let Some(encoder) = self.encoder.as_ref() else {
            return self.stats.packets_written;
        };
        let counters = encoder.counters();
        self.stats
            .packets_written
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
    /// Everything this session still holds counts: the pending input, an accepted input
    /// still inside the encoder ingress or a native call, and an outstanding end of
    /// stream. A session that simply has no new capture surface owns no work.
    fn work_outstanding(&self) -> bool {
        if self.pending.is_some() {
            return true;
        }
        let counters = match self.encoder.as_ref() {
            Some(encoder) => encoder.counters(),
            None => return false,
        };
        counters.queued_inputs > 0
            || counters.submitted_in_flight() > 0
            || (self.eos_requested && self.phase != VideoRecordingPhase::Drained)
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
            "The recording made no observable progress for {} ms while it still owned work: {} input(s) queued, {} native submission(s) in flight, {} packet(s) written, {} packet(s) held by the encoder, {} accepted input(s). The last native call was: {}. A native call inside the encoder or capture worker cannot be forcibly cancelled, so the recording is reported as stalled instead of being waited out.",
            elapsed.as_millis(),
            counters.queued_inputs,
            counters.submitted_in_flight(),
            self.stats.packets_written,
            counters.packets_held,
            counters.accepted_inputs,
            self.encoder
                .as_ref()
                .map(WindowsH264Encoder::native_call_phase)
                .unwrap_or_else(|| "no encoder session".to_string())
        )))
    }

    /// Releases the encoder, the capture session and the container writer, in that order,
    /// without finalizing anything. Idempotent.
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
        self.encoder = None;
        self.session = None;
        // Dropped, not finalized: the partial file stays explicitly incomplete and no
        // artifact is produced.
        self.writer = None;
        self.lookahead = None;
        self.pending = None;
        self.eos_requested = false;
        self.closed = true;
        self.phase = VideoRecordingPhase::Closed;
        self.stats.phase = VideoRecordingPhase::Closed;
        match combine_errors(
            combine_errors(encoder_failure, capture_failure),
            metadata_failure,
        ) {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl Drop for VideoRecording {
    fn drop(&mut self) {
        // Safety net only: joins the two native workers on this control thread and drops
        // the unfinalized container. It never detaches a worker and never produces an
        // artifact.
        let _ = self.release();
    }
}

#[cfg(test)]
mod encoder_signal_tests {
    use super::validate_encoder_signal;

    #[test]
    fn rejects_codec_signal_mismatch_before_native_start() {
        let h264 = "intel-mf-h264-d3d11:luid=0000000000000001";
        let hevc = "nvidia-nvenc-hevc-main10-d3d11:luid=0000000000000001";
        let av1 = "nvidia-nvenc-av1-d3d11:luid=0000000000000001";
        let av1_10 = "nvidia-nvenc-av1-main10-d3d11:luid=0000000000000001";
        assert!(validate_encoder_signal(false, Some(av1)).is_ok());
        assert!(validate_encoder_signal(true, Some(av1_10)).is_ok());
        assert!(validate_encoder_signal(true, Some(av1)).is_err());
        assert!(validate_encoder_signal(false, Some(av1_10)).is_err());
        assert!(validate_encoder_signal(true, Some(h264)).is_err());
        assert!(validate_encoder_signal(false, Some(hevc)).is_err());
        assert!(validate_encoder_signal(false, Some(h264)).is_ok());
        assert!(validate_encoder_signal(true, Some(hevc)).is_ok());
        assert!(validate_encoder_signal(true, None).is_ok());
        assert!(validate_encoder_signal(false, None).is_ok());
    }
}

/// Waits for the first completed capture surface within the preparation budget.
///
/// This is the one bounded wait of this module, and it is native preparation: a display
/// that never completes a surface is reported as an unavailable source.
fn wait_for_first_surface(
    session: &mut dyn CaptureSession<Surface = WindowsSurface>,
    cancel_start: &AtomicBool,
) -> Result<CapturedFrame<WindowsSurface>, RecorderError> {
    let deadline = Instant::now() + PREPARATION_WATCHDOG;
    loop {
        crate::recording_control::check_preparation_cancelled(cancel_start)?;
        if let Some(frame) = session.try_frame()? {
            return Ok(frame);
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
        let directory = std::env::temp_dir().join("freym-video-preparation-fix-002");
        std::fs::create_dir_all(&directory).unwrap();
        directory.join(format!(
            "{name}-{}.mkv",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    struct Capture;
    impl CaptureSession for Capture {
        type Surface = WindowsSurface;
        fn try_frame(&mut self) -> Result<Option<CapturedFrame<WindowsSurface>>, RecorderError> {
            panic!("cancelled preparation must not poll capture");
        }
        fn stop(&mut self) -> Result<(), RecorderError> {
            Ok(())
        }
    }

    #[test]
    fn fix002_clean_encoder_stop_keeps_other_errors_visible() {
        let expected = RecorderError {
            code: ErrorCode::FinalizationFailed,
            message: "The encoder stopped without completing the stream.".into(),
            recovery: RecoveryStatus::NoRecording,
            action: "Discard the incomplete stream.".into(),
        };
        assert!(preparation_encoder_result(Err(expected.clone())).is_ok());
        let mut cleanup = expected.clone();
        cleanup
            .message
            .push_str(" Teardown also reported a native failure.");
        assert_eq!(
            preparation_encoder_result(Err(cleanup.clone())),
            Err(cleanup)
        );
        let mut native = expected;
        native.code = ErrorCode::Internal;
        assert_eq!(preparation_encoder_result(Err(native.clone())), Err(native));
    }

    #[test]
    fn fix002_cancelled_first_surface_wait_has_no_capture_poll() {
        let error = wait_for_first_surface(&mut Capture, &AtomicBool::new(true))
            .err()
            .unwrap();
        assert_eq!(error.code, ErrorCode::InvalidTransition);
    }

    #[test]
    fn fix002_cancelled_video_entry_never_reaches_capture_or_output() {
        let output = fixture_output("never-video");
        let request = VideoRecordingRequest {
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
            clock: Arc::new(MasterClock::default()),
            origin: MediaTimestamp(0),
            output: output.clone(),
            camera: None,
            show_cursor: true,
            hdr_main10: false,
        };
        let error = VideoRecording::start_with_preview_cancellable(
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
