//! The desktop host's one recording owner (FRAME-0111B).
//!
//! Selects and owns a native recording session through one lifecycle:
//!
//! - one control thread owns the recording for its whole life. It is the only thread that
//!   prepares a native session, writes container bytes or joins a native worker, so the
//!   interface thread never does native preparation and never writes to disk;
//! - the interface thread reaches that thread through one *bounded* command channel. A
//!   full channel is reported as a busy control thread instead of an unbounded queue;
//! - [`frame_core::Recorder`] is the authoritative lifecycle. A command acknowledgement
//!   only means "the control thread accepted the command"; the state the interface sees
//!   is published by the control thread after the native owner really reached it;
//! - every published state and every event is produced under one lock, and the state is
//!   stored *before* the event that carries its sequence is emitted. A snapshot and the
//!   event stream therefore always agree about a sequence, and a client that subscribes
//!   before it snapshots can project the buffered events without a gap or a duplicate.
//!
//! Scope
//! -----
//!
//! A Windows display or canonical physical-pixel region is recorded at its own geometry.
//! Quality and FPS are frozen in a `SessionPlan`; fixed FPS admits only real captured
//! frames. Selected audio routes choose the A/V owner, while all Ignore routes choose
//! the single-video owner. Both share one clock and origin with their container.
//!
//! [`capabilities`] admits supported targets, encoders and output settings before a
//! native session opens. Unsupported configurations return a specific error.
//!
//! Failures
//! --------
//!
//! A native failure keeps its own [`RecorderError`] — code, message, recovery and action —
//! and is published as the authoritative `RecorderState::Failed`, which is also what a
//! later snapshot reports. Nothing here replaces a native error with a generic one, and
//! `EngineEvent::RecordingFinalized` is published only after a completed finalize; a
//! failure, an abort, a shutdown or a dropped owner never produces an artifact.
//!
//! Results and telemetry
//! ---------------------
//!
//! The one published state also carries the last telemetry and the last completed file, so
//! a snapshot refresh reports both instead of losing them:
//!
//! - `last_artifact` is filled only by a successful finalize and cleared when the next
//!   session prepares. It is the one place the interface can read a real file path and its
//!   container duration from, and it is never produced by a failure or an abort.
//! - `metrics` are published from this one control loop at most once per
//!   [`METRICS_INTERVAL`] (4 Hz). The elapsed time is read from the live session's own
//!   clock, and the two frame rates are real elapsed-window samples over the accepted
//!   owner's cumulative counters. Encoder ingress and packet-delivery gauges come from the
//!   live encoder snapshot. Everything this host does not measure — stage latencies, CPU,
//!   GPU, memory, A/V offset and dropped frames — is absent rather than zero. Elapsed wall
//!   time and the container's media duration stay separate observations.
//!
//! Teardown
//! --------
//!
//! [`RecordingControl::shutdown`] sets an out-of-band flag and joins the control thread,
//! so closing the window cannot leave a capture session, an encoder session or an open
//! container behind. The join is bounded by the native watchdogs inside the accepted
//! owner; a native call that never returns cannot be forcibly cancelled and is reported
//! instead of being hidden.
use std::collections::BTreeMap;
#[cfg(any(target_os = "windows", target_os = "macos", test))]
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
#[cfg(target_os = "windows")]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[cfg(target_os = "windows")]
use frame_audio::metering::MeterHub;
#[cfg(target_os = "windows")]
use frame_capture::{CaptureCancellation, CaptureContext, CaptureSession};
use frame_clock::{MasterClock, MediaTimestamp};
#[cfg(target_os = "windows")]
use frame_contracts::AudioLevelsSnapshot;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use frame_contracts::Dimensions;
#[cfg(any(target_os = "linux", test))]
use frame_contracts::EncoderPreference;
#[cfg(target_os = "windows")]
use frame_contracts::FpsMode;
use frame_contracts::{
    AudioRoute, AudioRouteSelection, AudioSource, AudioSourceControls, AudioSourceId,
    CaptureTarget, DisplayInfo, EngineCapabilities, EngineEvent, EngineWarning, ErrorCode,
    EventEnvelope, FrameRateStatus, QueueMetrics, RecorderError, RecorderMetrics, RecorderState,
    RecordingArtifact, RecordingRequest, RecoveryStatus, ReplaySaveResult, ReplaySettings,
};
use frame_core::{Recorder, SessionId};
use frame_metrics::RateSampler;
use frame_settings::SessionPlan;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use frame_settings::{EncoderCandidate, EncoderLimits, NominalRate, PlanInputs};
#[cfg(target_os = "windows")]
use frame_settings::{EncoderPreset, FrameRatePlan, RateControl, validate_region};

/// The event name the frontend already subscribes to. One envelope is one event.
pub const EVENT_NAME: &str = "engine-event";

/// The bounded command capacity.
///
/// Four commands are more than one interface can produce without waiting: a start, a stop,
/// a reset and a shutdown. The point of the bound is that the queue can never grow, and
/// that a full channel is a reported busy state rather than hidden latency.
const COMMAND_CAPACITY: usize = 4;

#[cfg(target_os = "windows")]
const PREVIEW_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PreviewQuality {
    #[default]
    Native,
    #[serde(rename = "p1080")]
    P1080,
    #[serde(rename = "p720")]
    P720,
}

#[cfg(target_os = "windows")]
impl PreviewQuality {
    fn config(self) -> frame_windows::preview::PreviewConfig {
        use frame_windows::preview::PreviewConfig;
        match self {
            Self::Native => PreviewConfig::default(),
            Self::P1080 => PreviewConfig::reduced(1920, 1080),
            Self::P720 => PreviewConfig::reduced(1280, 720),
        }
    }
}

#[cfg(target_os = "windows")]
#[derive(Default)]
struct PreviewPublication {
    hub: Option<Arc<frame_windows::preview::PreviewHub>>,
    error: Option<RecorderError>,
}

/// How long the control thread waits for a command while a recording is live.
///
/// Capture publication, encoder completion/credit release and commands wake the
/// owner immediately. This fallback services audio and watchdogs on quiet sources;
/// it is not a frame-rate limit or a delay imposed after each frame.
const LIVE_POLL_INTERVAL: Duration = Duration::from_millis(1);

/// How long the control thread waits for a command while it owns no recording. A command
/// still wakes it immediately; the longer wait only keeps an idle host quiet.
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How often the control thread may publish one `Metrics` event: 250 ms is 4 Hz.
///
/// Telemetry is published from the same control loop that advances the native owner, so it
/// can never pace the recording: one step still only waits for a command or one poll
/// interval, and a metrics event is emitted at most once every 250 ms. The elapsed value
/// is read from the live session's own clock, never from the metrics cadence.
const METRICS_INTERVAL: Duration = Duration::from_millis(250);

/// The most precise denominator considered when the display's reported refresh rate is
/// turned back into an exact rational. 1001 covers the NTSC-derived modes
/// (`60000/1001`) as well as whole-hertz and thousandth-of-a-hertz modes.
#[cfg(any(target_os = "windows", target_os = "macos", test))]
const MAX_RATE_DENOMINATOR: u32 = 1001;

/// How far the recovered rational may sit from the reported rate, in hertz.
///
/// The contract carries the rate as an `f64`, so the exact rational has to be recovered
/// from it. A rate that cannot be recovered inside this tolerance is reported instead of
/// being rounded into a plausible-looking nominal rate.
#[cfg(any(target_os = "windows", target_os = "macos", test))]
const NOMINAL_RATE_TOLERANCE_HZ: f64 = 0.001;

/// What the Windows host can actually record today, in the words the interface shows.
#[cfg(target_os = "windows")]
const WINDOWS_SCOPE_REASON: &str = "Display/region recording and SDR GPU preview remain available. Eligible SDR native windows and explicitly identified windowed/borderless game windows use the isolated WGC helper with adapter-matched NVENC and MKV; window GPU preview shares those capture slots. WGC owns window cursor capture and honors the cursor preference. Window camera and HDR are unavailable. Exclusive fullscreen and injection are unsupported.";

#[cfg(target_os = "windows")]
static PREVIEW_GENERATION: AtomicU64 = AtomicU64::new(1);
#[cfg(target_os = "windows")]
static LEVEL_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Reported instead when there is no display to record.
#[cfg(any(target_os = "windows", target_os = "macos"))]
const NO_DISPLAY_REASON: &str =
    "No display is attached, so there is no source to record on this host.";

/// Reported on a platform that has no native recording owner at all.
#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
const NON_WINDOWS_REASON: &str =
    "Native recording is implemented for Windows displays only on this host.";

// -------------------------------------------------------------------------- errors

fn invalid_request_error(message: String, action: &str) -> RecorderError {
    RecorderError {
        code: ErrorCode::InvalidRequest,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: action.to_string(),
    }
}

fn unsupported_error(message: String, action: &str) -> RecorderError {
    RecorderError {
        code: ErrorCode::NotImplemented,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: action.to_string(),
    }
}

fn invalid_output_error(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::InvalidOutputPath,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: "Choose an accessible recording folder in More Options and retry.".to_string(),
    }
}

fn invalid_transition_error(message: String) -> RecorderError {
    RecorderError {
        code: ErrorCode::InvalidTransition,
        message,
        recovery: RecoveryStatus::NoRecording,
        action: "Follow the recording lifecycle: start while idle, stop while recording, and reset only a failed recording."
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

// ------------------------------------------------------------------------ events

/// Where one published envelope goes.
///
/// The host library owns the recording and the binary owns the window, so this trait is
/// the one seam between them: the desktop window supplies an application event sink,
/// while an example or a test can observe the same stream without a window.
pub trait EngineEventSink: Send + Sync + 'static {
    fn publish(&self, envelope: EventEnvelope);
}

/// The published recorder state: the sequence, the session, the state itself, the last
/// measured metrics and the last finalized artifact.
///
/// These always belong to the same instant, which is why they are published together
/// under one lock and handed to a snapshot as one value. `last_artifact` is what makes a
/// finished recording survive a snapshot refresh: it is host state, not a one-off event.
#[derive(Clone, Debug, PartialEq)]
pub struct PublishedState {
    pub sequence: u32,
    pub session_id: Option<String>,
    pub state: RecorderState,
    /// The last telemetry this host measured for the live session, or `null` before a
    /// session started and after a new one began preparing.
    pub metrics: Option<RecorderMetrics>,
    /// The last recording that really finalized, cleared when a new session prepares.
    pub last_artifact: Option<RecordingArtifact>,
    pub replay_settings: ReplaySettings,
    pub replay_active: bool,
    pub replay_only: bool,
    pub last_replay: Option<ReplaySaveResult>,
    pub plan_decisions: Vec<String>,
    pub audio_sources: Vec<AudioSource>,
    pub saved_routes: BTreeMap<AudioSourceId, AudioRoute>,
    pub saved_mutes: BTreeMap<AudioSourceId, bool>,
    pub saved_audio_controls: BTreeMap<AudioSourceId, AudioSourceControls>,
}

impl Default for PublishedState {
    fn default() -> Self {
        Self {
            sequence: 0,
            session_id: None,
            state: RecorderState::Idle,
            metrics: None,
            last_artifact: None,
            replay_settings: ReplaySettings::default(),
            replay_active: false,
            replay_only: false,
            last_replay: None,
            plan_decisions: Vec::new(),
            audio_sources: Vec::new(),
            saved_routes: BTreeMap::new(),
            saved_mutes: BTreeMap::new(),
            saved_audio_controls: BTreeMap::new(),
        }
    }
}

// ----------------------------------------------------------------------- commands

/// One command for the control thread. It is the only way to reach a recording.
enum ControlCommand {
    /// Begin one recording into one new MKV path. The path is already derived and checked
    /// by [`RecordingControl::start`], which runs on the caller's thread.
    Start {
        request: Box<RecordingRequest>,
        output: PathBuf,
        countdown_seconds: u8,
        replay_only: bool,
    },
    /// Close admission and drain towards the artifact.
    Stop,
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    Pause,
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    Resume,
    #[cfg(target_os = "windows")]
    OpenPreview {
        target: CaptureTarget,
        quality: PreviewQuality,
        show_cursor: bool,
        reply: SyncSender<Result<(), RecorderError>>,
    },
    #[cfg(target_os = "windows")]
    OpenGpuPreview {
        target: CaptureTarget,
        show_cursor: bool,
        reply: SyncSender<Result<(), RecorderError>>,
    },
    #[cfg(target_os = "windows")]
    ClosePreview {
        reply: SyncSender<Result<(), RecorderError>>,
    },
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    ConfigureReplay(ReplaySettings),
    ConfigureInteractionMetadata(bool),
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    SaveReplay {
        path: PathBuf,
        duration_seconds: u32,
    },
    ReportError(RecorderError),
    /// Return a failed recording to idle after its resources are released.
    Reset,
    UpdateAudioControls {
        source_id: AudioSourceId,
        controls: AudioSourceControls,
    },
    UpdateMute {
        source_id: AudioSourceId,
        muted: bool,
    },
    UpdateRoute {
        source_id: AudioSourceId,
        route: AudioRoute,
    },
    #[cfg(target_os = "windows")]
    StartLevels {
        ids: Vec<AudioSourceId>,
        reply: SyncSender<Result<(), RecorderError>>,
    },
    #[cfg(target_os = "windows")]
    StopLevels {
        reply: SyncSender<Result<(), RecorderError>>,
    },
    /// Only wakes a waiting control thread: [`RecordingControl::shutdown`] sets the flag
    /// that is the actual signal, so a full channel can never lose the request.
    Shutdown,
}

// ------------------------------------------------------------------- capabilities

/// The capabilities this host can honestly report for the enumerated displays.
///
/// `can_record` is true on Windows exactly when a display exists to record, because the
/// recording scope above is wired up; every flag outside that scope stays false and the
/// reason names the scope instead of a plan. The native owners share a pause timeline.
#[cfg(target_os = "windows")]
pub fn capabilities(displays: &[DisplayInfo]) -> EngineCapabilities {
    let can_record = !displays.is_empty();
    let wgc_nvenc = can_record
        && frame_windows::enumerate_native_adapters()
            .is_ok_and(|adapters| adapters.iter().any(|adapter| adapter.vendor_id == 0x10DE));
    EngineCapabilities {
        can_record,
        can_pause: can_record,
        can_replay: can_record,
        can_hotkeys: true,
        camera_overlay: true,
        live_preview: true,
        audio_levels: true,
        fixed_fps: true,
        display_capture: true,
        window_capture: wgc_nvenc,
        region_capture: true,
        game_capture: wgc_nvenc,
        per_app_audio: frame_windows::windows_build_number().is_ok_and(|build| build >= 20348),
        microphone: true,
        reason: Some(
            if can_record {
                WINDOWS_SCOPE_REASON
            } else {
                NO_DISPLAY_REASON
            }
            .to_string(),
        ),
    }
}

/// Linux exposes only the portal monitor path after its runtime probes pass.
#[cfg(target_os = "linux")]
pub fn capabilities(displays: &[DisplayInfo]) -> EngineCapabilities {
    let available = !displays.is_empty() && frame_linux::probe_encoder().ok().flatten().is_some();
    EngineCapabilities {
        can_record: available,
        can_pause: false,
        can_replay: false,
        can_hotkeys: true,
        camera_overlay: false,
        live_preview: false,
        audio_levels: false,
        fixed_fps: false,
        display_capture: available,
        window_capture: false,
        region_capture: false,
        game_capture: false,
        per_app_audio: frame_linux::application_audio_supported(),
        microphone: true,
        reason: Some(if available {
            "Portal screen picker, selected PipeWire microphones, optional isolated PulseAudio/PipeWire-Pulse application streams, VA-API, Opus and MKV are available. X11 uses the same portal path. Pause, replay, windows and region are unavailable."
        } else {
            "A ScreenCast portal and the VA-API H.264/GStreamer path are required for Linux recording."
        }.into()),
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
pub fn capabilities(_displays: &[DisplayInfo]) -> EngineCapabilities {
    EngineCapabilities {
        can_record: false,
        can_pause: false,
        can_replay: false,
        can_hotkeys: false,
        camera_overlay: false,
        live_preview: false,
        audio_levels: false,
        fixed_fps: false,
        display_capture: false,
        window_capture: false,
        region_capture: false,
        game_capture: false,
        per_app_audio: false,
        microphone: false,
        reason: Some(NON_WINDOWS_REASON.to_string()),
    }
}

#[cfg(target_os = "macos")]
pub fn capabilities(displays: &[DisplayInfo]) -> EngineCapabilities {
    let can_record = !displays.is_empty();
    let screen_authorized = frame_macos::screen_authorized();
    EngineCapabilities {
        can_record,
        can_pause: false,
        can_replay: false,
        can_hotkeys: true,
        camera_overlay: false,
        live_preview: false,
        audio_levels: false,
        fixed_fps: false,
        display_capture: true,
        window_capture: screen_authorized,
        region_capture: false,
        game_capture: false,
        per_app_audio: screen_authorized,
        microphone: true,
        reason: Some(if !can_record {
            NO_DISPLAY_REASON
        } else if !screen_authorized {
            "macOS 14+: press Record on a display to request Screen Recording access. Window and application sources appear after access is granted."
        } else {
            "macOS 14+: ScreenCaptureKit display/window, VideoToolbox H.264, selected microphone and filtered application audio. Camera, HDR, pause and replay are unavailable."
        }.into()),
    }
}

// ---------------------------------------------------------------------- the owner

/// The host's one recording owner.
///
/// It owns the bounded command channel, the published state and the join handle of the
/// one control thread. It is `Send + Sync`, holds no native handle and performs no native
/// preparation: `start`, `stop` and `reset` only make one bounded decision and enqueue one
/// command.
pub struct RecordingControl {
    commands: SyncSender<ControlCommand>,
    command_wake: Option<std::thread::Thread>,
    /// Serializes command admission so Stop cannot overtake a reserved Start.
    command_gate: Arc<Mutex<()>>,
    shutdown: Arc<AtomicBool>,
    shared: Arc<Mutex<PublishedState>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    start_pending: Arc<AtomicBool>,
    /// A queued Stop closes admission to later pause/resume commands immediately.
    stop_pending: AtomicBool,
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    cancel_start: Arc<AtomicBool>,
    #[cfg(target_os = "windows")]
    preview: Arc<Mutex<PreviewPublication>>,
    #[cfg(target_os = "windows")]
    meter: Arc<MeterHub>,
}

impl RecordingControl {
    /// Opt-in pointer metadata for the next recording. No text or keyboard capture.
    pub fn configure_interaction_metadata(&self, enabled: bool) -> Result<(), RecorderError> {
        self.send(ControlCommand::ConfigureInteractionMetadata(enabled))
    }
    /// Starts the one control thread. The sink receives every published envelope.
    pub fn new(sink: Arc<dyn EngineEventSink>) -> Self {
        Self::new_inner(
            sink,
            #[cfg(all(test, target_os = "windows"))]
            None,
        )
    }

    fn new_inner(
        sink: Arc<dyn EngineEventSink>,
        #[cfg(all(test, target_os = "windows"))] preparation: Option<PreparationSeam>,
    ) -> Self {
        let (commands, receiver) = sync_channel(COMMAND_CAPACITY);
        let shutdown = Arc::new(AtomicBool::new(false));
        let start_pending = Arc::new(AtomicBool::new(false));
        let command_gate = Arc::new(Mutex::new(()));
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        let cancel_start = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Mutex::new(PublishedState::default()));
        #[cfg(target_os = "windows")]
        let preview = Arc::new(Mutex::new(PreviewPublication::default()));
        #[cfg(target_os = "windows")]
        let meter = Arc::new(MeterHub::default());
        // The worker is constructed *inside* the thread, not moved into it: the accepted
        // native owner is deliberately not `Send` — it owns a capture session, an encoder
        // session and a container writer that must live and be released on one thread — so
        // it is created on the thread that owns it for its whole life and never crosses a
        // thread boundary. A thread that cannot be created leaves the channel without a
        // receiver, so every command reports that the control thread is not running
        // instead of pretending a recording could start.
        let handle = std::thread::Builder::new()
            .name("frame-recording-control".to_string())
            .spawn({
                let sink = Arc::clone(&sink);
                let shared = Arc::clone(&shared);
                let shutdown = Arc::clone(&shutdown);
                let start_pending = Arc::clone(&start_pending);
                let command_gate = Arc::clone(&command_gate);
                #[cfg(any(target_os = "windows", target_os = "linux"))]
                let cancel_start = Arc::clone(&cancel_start);
                #[cfg(target_os = "windows")]
                let preview = Arc::clone(&preview);
                #[cfg(target_os = "windows")]
                let meter = Arc::clone(&meter);
                move || {
                    ControlWorker {
                        sink,
                        shared,
                        shutdown,
                        start_pending,
                        command_gate,
                        #[cfg(any(target_os = "windows", target_os = "linux"))]
                        cancel_start,
                        commands: receiver,
                        recorder: Recorder::default(),
                        session: None,
                        recording: None,
                        countdown: None,
                        interaction_metadata: false,
                        #[cfg(all(test, target_os = "windows"))]
                        preparation,
                        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
                        replay_settings: ReplaySettings::default(),
                        replay_only: false,
                        #[cfg(target_os = "windows")]
                        replay_worker: None,
                        storage_monitor: crate::recording_files::StorageMonitor::new(),
                        storage_directory: None,
                        last_metrics: None,
                        #[cfg(target_os = "windows")]
                        preview,
                        #[cfg(target_os = "windows")]
                        idle_preview: None,
                        #[cfg(target_os = "windows")]
                        preview_target: None,
                        #[cfg(target_os = "windows")]
                        preview_quality: PreviewQuality::default(),
                        #[cfg(target_os = "windows")]
                        preview_gpu_mode: false,
                        #[cfg(target_os = "windows")]
                        preview_show_cursor: true,
                        #[cfg(target_os = "windows")]
                        meter,
                        #[cfg(target_os = "windows")]
                        level_test: None,
                    }
                    .run()
                }
            })
            .ok();
        let command_wake = handle.as_ref().map(|worker| worker.thread().clone());
        Self {
            commands,
            command_wake,
            command_gate,
            shutdown,
            start_pending,
            stop_pending: AtomicBool::new(false),
            #[cfg(any(target_os = "windows", target_os = "linux"))]
            cancel_start,
            shared,
            worker: Mutex::new(handle),
            #[cfg(target_os = "windows")]
            preview,
            #[cfg(target_os = "windows")]
            meter,
        }
    }

    #[cfg(target_os = "windows")]
    pub fn audio_levels(&self) -> AudioLevelsSnapshot {
        self.meter.snapshot()
    }

    #[cfg(target_os = "windows")]
    pub fn start_audio_level_test(&self, ids: Vec<AudioSourceId>) -> Result<(), RecorderError> {
        let (reply, answer) = sync_channel(1);
        self.send(ControlCommand::StartLevels { ids, reply })?;
        answer.recv().map_err(|_| {
            internal_error(
                "Audio level test control thread closed before acknowledging startup.".into(),
            )
        })?
    }

    #[cfg(target_os = "windows")]
    pub fn stop_audio_level_test(&self) -> Result<(), RecorderError> {
        let (reply, answer) = sync_channel(1);
        self.send(ControlCommand::StopLevels { reply })?;
        answer.recv().map_err(|_| {
            internal_error(
                "Audio level test control thread closed before acknowledging stop.".into(),
            )
        })?
    }

    /// The preview hub of the current display recording, if it prepared successfully.
    #[cfg(target_os = "windows")]
    pub fn preview_hub(&self) -> Option<Arc<frame_windows::preview::PreviewHub>> {
        self.preview.lock().ok()?.hub.clone()
    }

    /// An idle preview failure is separate from the recorder and audio level state.
    #[cfg(target_os = "windows")]
    pub fn preview_error(&self) -> Option<RecorderError> {
        self.preview.lock().ok()?.error.clone()
    }

    /// Prepares a capture-only preview on the one native control thread.
    #[cfg(target_os = "windows")]
    pub fn open_preview(
        &self,
        target: CaptureTarget,
        quality: PreviewQuality,
        show_cursor: bool,
    ) -> Result<(), RecorderError> {
        let (reply, answer) = sync_channel(1);
        self.send(ControlCommand::OpenPreview {
            target,
            quality,
            show_cursor,
            reply,
        })?;
        answer
            .recv_timeout(PREVIEW_COMMAND_TIMEOUT)
            .map_err(|error| {
                internal_error(format!(
                    "Preview startup was not acknowledged within 15 seconds: {error}"
                ))
            })?
    }

    /// Starts the idle display capture with its GPU sink attached before the
    /// first accepted surface. A live recording uses its existing capture hub.
    #[cfg(target_os = "windows")]
    pub fn open_gpu_preview(
        &self,
        target: CaptureTarget,
        show_cursor: bool,
    ) -> Result<(), RecorderError> {
        let (reply, answer) = sync_channel(1);
        self.send(ControlCommand::OpenGpuPreview {
            target,
            show_cursor,
            reply,
        })?;
        answer
            .recv_timeout(PREVIEW_COMMAND_TIMEOUT)
            .map_err(|error| {
                internal_error(format!("GPU preview startup was not acknowledged: {error}"))
            })?
    }

    /// Closes the idle capture owner. A live recording keeps its own preview hub.
    #[cfg(target_os = "windows")]
    pub fn close_preview(&self) -> Result<(), RecorderError> {
        let (reply, answer) = sync_channel(1);
        self.send(ControlCommand::ClosePreview { reply })?;
        answer
            .recv_timeout(PREVIEW_COMMAND_TIMEOUT)
            .map_err(|error| {
                internal_error(format!(
                    "Preview close was not acknowledged within 15 seconds: {error}"
                ))
            })?
    }

    /// The published sequence, session and state, all from one instant.
    pub fn published(&self) -> PublishedState {
        self.shared
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    /// A source chooser may run only while no recording start is queued.
    /// The chooser itself is read-only; Record still revalidates the selected ID.
    pub fn source_selection_available(&self) -> Result<(), RecorderError> {
        let _admission = self
            .command_gate
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if self.start_pending.load(Ordering::Acquire)
            || self.published().state != RecorderState::Idle
        {
            return Err(invalid_transition_error(
                "Window selection is available only while the recorder is idle.".into(),
            ));
        }
        Ok(())
    }

    /// Reconciles a fresh native source list with the route choices held by this owner.
    pub fn observe_sources(&self, mut sources: Vec<AudioSource>) -> PublishedState {
        let mut shared = self
            .shared
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        for source in &mut sources {
            source.muted = shared.saved_mutes.get(&source.id).copied().unwrap_or(false);
            source.controls = shared
                .saved_audio_controls
                .get(&source.id)
                .copied()
                .unwrap_or_default();
            source.route = shared
                .saved_routes
                .get(&source.id)
                .copied()
                .unwrap_or(AudioRoute::Ignore);
        }
        shared.audio_sources = sources;
        // Routes and their event sequence must come from the same locked observation.
        // Otherwise a route event between this call and published() could be skipped
        // by the client while its snapshot still contains the previous route.
        shared.clone()
    }

    /// Seeds the source selection from the versioned settings file at startup.
    pub fn restore_routes(&self, routes: &[AudioRouteSelection]) {
        let mut shared = self
            .shared
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        shared.saved_audio_controls = routes
            .iter()
            .map(|r| (r.source_id.clone(), r.controls))
            .collect();
        shared.saved_mutes = routes
            .iter()
            .map(|s| (s.source_id.clone(), s.muted))
            .collect();
        shared.saved_routes = routes
            .iter()
            .map(|selection| (selection.source_id.clone(), selection.route))
            .collect();
    }

    /// Processing/reconnect selection is frozen for the next recording.
    pub fn update_audio_controls(
        &self,
        source_id: AudioSourceId,
        controls: AudioSourceControls,
    ) -> Result<(), RecorderError> {
        controls.validate()?;
        let state = self.published();
        if !matches!(state.state, RecorderState::Idle)
            || !state.audio_sources.iter().any(|s| s.id == source_id)
        {
            return Err(RecorderError::invalid(
                "Stop recording before changing source processing.",
            ));
        }
        self.send(ControlCommand::UpdateAudioControls {
            source_id,
            controls,
        })
    }

    /// Mute is independent of route selection and applies at the next PCM quantum.
    pub fn update_mute(&self, source_id: AudioSourceId, muted: bool) -> Result<(), RecorderError> {
        let shared = self.published();
        if !matches!(
            shared.state,
            RecorderState::Idle | RecorderState::Recording | RecorderState::Paused
        ) {
            return Err(invalid_transition_error(
                "Audio mute is unavailable during a recording transition.".into(),
            ));
        }
        if !shared.audio_sources.iter().any(|s| s.id == source_id) {
            return Err(RecorderError::invalid(
                "The selected audio source is unavailable.",
            ));
        }
        self.send(ControlCommand::UpdateMute { source_id, muted })
    }

    /// Routes are editable only while idle. The control thread publishes the changed list.
    pub fn update_route(
        &self,
        source_id: AudioSourceId,
        route: AudioRoute,
    ) -> Result<(), RecorderError> {
        let shared = self.published();
        if !matches!(shared.state, RecorderState::Idle) {
            return Err(invalid_transition_error(
                "Audio routes can be changed only while idle.".into(),
            ));
        }
        if !shared
            .audio_sources
            .iter()
            .any(|source| source.id == source_id)
        {
            return Err(invalid_request_error(
                format!("Audio source {} is no longer available.", source_id.0),
                "Refresh sources and choose an available audio source.",
            ));
        }
        self.send(ControlCommand::UpdateRoute { source_id, route })
    }

    /// Prepares one recording request and hands it to the control thread.
    ///
    /// The scope and the output path are checked here so an unsupported request is
    /// reported to the caller with its own error without disturbing a live recording, and
    /// the control thread checks the same scope again before it trusts the command. A
    /// returned `Ok` means the control thread accepted the command, not that a native
    /// session exists yet: readiness arrives as a published state.
    pub fn start(&self, request: RecordingRequest) -> Result<(), RecorderError> {
        self.start_with_countdown(request, 0)
    }

    /// Reserves the sole owner immediately, then delays native preparation by 3/5/10 s.
    pub fn start_with_countdown(
        &self,
        request: RecordingRequest,
        seconds: u8,
    ) -> Result<(), RecorderError> {
        if !matches!(seconds, 0 | 3 | 5 | 10) {
            return Err(invalid_request_error(
                "Countdown must be 0, 3, 5 or 10 seconds.".into(),
                "Choose a supported countdown.",
            ));
        }
        self.start_mode(request, seconds, false)
    }

    /// Starts continuous replay using the same capture/encoder admission gates.
    pub fn start_replay(&self, request: RecordingRequest) -> Result<(), RecorderError> {
        if !cfg!(target_os = "windows") {
            return Err(RecorderError::invalid(
                "Replay is unavailable on this platform.",
            ));
        }
        self.start_mode(request, 0, true)
    }

    pub fn stop_replay(&self) -> Result<(), RecorderError> {
        if !self.published().replay_only {
            return Err(invalid_transition_error(
                "No replay-only capture is active.".into(),
            ));
        }
        self.stop()
    }

    fn start_mode(
        &self,
        request: RecordingRequest,
        seconds: u8,
        replay_only: bool,
    ) -> Result<(), RecorderError> {
        validate_scope(&request)?;
        let output = output_path(&request)?;
        let _admission = self
            .command_gate
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if self.start_pending.swap(true, Ordering::AcqRel) {
            return Err(invalid_transition_error(
                "A recording start is already pending on this host.".into(),
            ));
        }
        // Reserve first, then read state: reading Idle before the worker publishes
        // Preparing could otherwise admit a second Start during preview teardown.
        if !matches!(self.published().state, RecorderState::Idle) {
            self.start_pending.store(false, Ordering::Release);
            return Err(invalid_transition_error(
                "A recording is already active on this host, so a second one cannot start."
                    .to_string(),
            ));
        }
        self.stop_pending.store(false, Ordering::Release);
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        self.cancel_start.store(false, Ordering::Release);
        let outcome = self.send(ControlCommand::Start {
            request: Box::new(request),
            output,
            countdown_seconds: seconds,
            replay_only,
        });
        if outcome.is_err() {
            self.start_pending.store(false, Ordering::Release);
        }
        outcome
    }

    /// Closes admission and drains towards the artifact.
    ///
    /// It is valid while the lifecycle is preparing, recording or paused, exactly as the
    /// accepted lifecycle defines a stop.
    pub fn stop(&self) -> Result<(), RecorderError> {
        let _admission = self
            .command_gate
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let state = self.published().state;
        match state {
            RecorderState::Preparing | RecorderState::Recording | RecorderState::Paused => {
                if self.stop_pending.swap(true, Ordering::AcqRel) {
                    return Ok(());
                }
                #[cfg(any(target_os = "windows", target_os = "linux"))]
                if state == RecorderState::Preparing {
                    self.cancel_start.store(true, Ordering::Release);
                }
                let outcome = self.send(ControlCommand::Stop);
                if outcome.is_err() {
                    self.stop_pending.store(false, Ordering::Release);
                }
                outcome
            }
            RecorderState::Idle if self.start_pending.load(Ordering::Acquire) => {
                if self.stop_pending.swap(true, Ordering::AcqRel) {
                    return Ok(());
                }
                #[cfg(any(target_os = "windows", target_os = "linux"))]
                self.cancel_start.store(true, Ordering::Release);
                let outcome = self.send(ControlCommand::Stop);
                if outcome.is_err() {
                    self.stop_pending.store(false, Ordering::Release);
                }
                outcome
            }
            _ => Err(invalid_transition_error(
                "There is no active recording to stop.".to_string(),
            )),
        }
    }

    pub fn pause(&self) -> Result<(), RecorderError> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            Err(unsupported_error(
                "Pause is unavailable on this platform.".into(),
                "Use Stop to finish the recording.",
            ))
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _admission = self
                .command_gate
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if self.stop_pending.load(Ordering::Acquire) {
                return Err(invalid_transition_error("Recording is stopping.".into()));
            }
            if self.published().state != RecorderState::Recording {
                return Err(invalid_transition_error("Recording is not running.".into()));
            }
            self.send(ControlCommand::Pause)
        }
    }

    pub fn resume(&self) -> Result<(), RecorderError> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            Err(unsupported_error(
                "Resume is unavailable on this platform.".into(),
                "Start a new recording.",
            ))
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _admission = self
                .command_gate
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if self.stop_pending.load(Ordering::Acquire) {
                return Err(invalid_transition_error("Recording is stopping.".into()));
            }
            if self.published().state != RecorderState::Paused {
                return Err(invalid_transition_error("Recording is not paused.".into()));
            }
            self.send(ControlCommand::Resume)
        }
    }

    /// Returns to idle.
    ///
    /// A failed recording is reset after its native resources are released, which the
    /// control thread owns. An idle host is accepted too: a request that was refused
    /// before it began a session is reported as failed by the interface, and resetting it
    /// only republishes the idle state this host is already in.
    pub fn reset(&self) -> Result<(), RecorderError> {
        match self.published().state {
            RecorderState::Failed(_) | RecorderState::Idle => self.send(ControlCommand::Reset),
            _ => Err(invalid_transition_error(
                "A recording in progress cannot be reset; stop it first.".to_string(),
            )),
        }
    }

    pub fn configure_replay(&self, settings: ReplaySettings) -> Result<(), RecorderError> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let _ = settings;
            Err(unsupported_error(
                "Replay is unavailable on this platform.".into(),
                "Record a new MKV instead.",
            ))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            replay_limit(settings)?;
            if !matches!(self.published().state, RecorderState::Idle) {
                return Err(invalid_transition_error(
                    "Stop capture before changing replay limits.".into(),
                ));
            }
            self.send(ControlCommand::ConfigureReplay(settings))
        }
    }

    /// Queues a snapshot for the background disk worker. `ReplaySaved` or `ReplaySaveFailed` is
    /// published after the file operation finishes; this acknowledgement is not success.
    pub fn save_replay(&self, path: PathBuf, duration_seconds: u32) -> Result<(), RecorderError> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let _ = (path, duration_seconds);
            Err(unsupported_error(
                "Replay is unavailable on this platform.".into(),
                "Record a new MKV instead.",
            ))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            if !matches!(
                self.published().state,
                RecorderState::Recording | RecorderState::Paused
            ) {
                return Err(invalid_transition_error(
                    "Replay requires an active recording.".into(),
                ));
            }
            if path
                .extension()
                .is_none_or(|ext| !ext.eq_ignore_ascii_case("mkv"))
            {
                return Err(invalid_output_error(
                    "Replay output must be a .mkv path.".into(),
                ));
            }
            if !path.is_absolute() {
                return Err(invalid_output_error(
                    "Replay output must be an absolute .mkv path.".into(),
                ));
            }
            if duration_seconds == 0 || duration_seconds > 600 {
                return Err(invalid_request_error(
                    "Replay duration must be 1–600 seconds.".into(),
                    "Choose a supported replay duration.",
                ));
            }
            self.send(ControlCommand::SaveReplay {
                path,
                duration_seconds,
            })
        }
    }

    pub fn report_command_error(&self, error: RecorderError) {
        let _ = self.send(ControlCommand::ReportError(error));
    }

    /// Stops and joins the one control thread.
    ///
    /// Idempotent, and the safety net a closing window needs: the control thread releases
    /// a live recording without finalizing it — a shutdown never produces an artifact —
    /// and joins the native workers it owns before this returns. The join cannot be
    /// forcibly interrupted, so a native call that never returns blocks it; that is
    /// reported as failed evidence rather than hidden by detaching the worker.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        self.cancel_start.store(true, Ordering::Release);
        // Wakes a control thread that is waiting for a command. The flag is the signal, so
        // a full channel cannot lose this request.
        let _ = self.commands.try_send(ControlCommand::Shutdown);
        if let Some(wake) = &self.command_wake {
            wake.unpark();
        }
        let handle = self
            .worker
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }

    fn send(&self, command: ControlCommand) -> Result<(), RecorderError> {
        match self.commands.try_send(command) {
            Ok(()) => {
                if let Some(wake) = &self.command_wake { wake.unpark(); }
                Ok(())
            },
            Err(TrySendError::Full(_)) => Err(internal_error(
                "The recording control thread has not accepted the previous commands yet, so this one was not queued."
                    .to_string(),
            )),
            Err(TrySendError::Disconnected(_)) => Err(internal_error(
                "The recording control thread is not running, so no command can be delivered."
                    .to_string(),
            )),
        }
    }
}

impl Drop for RecordingControl {
    fn drop(&mut self) {
        // Safety net only: a window that is closing must not leave the recording worker,
        // its capture session or its open container behind.
        self.shutdown();
    }
}

// ------------------------------------------------------------------- request rules

/// The one recording scope this host implements.
///
/// Everything outside it is refused with its own error. A preference that cannot be
/// applied — a fixed frame rate, another resolution, another encoder, any audio route
/// other than `Ignore` — is *rejected*, never quietly ignored, because a recording that
/// silently drops a user's choice would misrepresent what it produced.
fn validate_scope(request: &RecordingRequest) -> Result<(), RecorderError> {
    frame_settings::audio_admission::validate_audio_topology(&request.audio_routes)?;
    frame_settings::validate_request(request)?;
    #[cfg(target_os = "linux")]
    {
        if !matches!(&request.target, CaptureTarget::Display(id) if id.0 == frame_linux::PORTAL_PICKER_ID)
            || !matches!(request.resolution, frame_contracts::Resolution::Native)
            || !matches!(request.fps_mode, frame_contracts::FpsMode::AutoMatchSource)
            || request.encoder != EncoderPreference::Auto
            || request.camera.is_some()
        {
            return Err(unsupported_error(
                "Linux records a portal-selected screen at native resolution and source pace, with automatic VA-API encoding and no camera overlay.".into(),
                "Choose the portal picker, native resolution, source FPS and automatic encoder.",
            ));
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        if let Some(camera) = &request.camera
            && (camera.device_id.trim().is_empty()
                || camera.placement.width == 0
                || camera.placement.height == 0)
        {
            return Err(invalid_request_error(
                "Choose a camera device and a nonempty overlay placement.".into(),
                "Select a connected camera, its native format and a placement inside the recording.",
            ));
        }
        #[cfg(target_os = "macos")]
        if !matches!(
            request.target,
            CaptureTarget::Display(_) | CaptureTarget::Window(_)
        ) {
            return Err(unsupported_error(
                "macOS records a display or window in this version.".into(),
                "Choose a display or window.",
            ));
        }
        #[cfg(target_os = "windows")]
        if matches!(
            request.target,
            CaptureTarget::Window(_) | CaptureTarget::Game(_)
        ) && request.camera.is_some()
        {
            return Err(unsupported_error(
                "Isolated WGC records an SDR native window without camera compositing.".into(),
                "Turn off camera, or choose a display for camera compositing.",
            ));
        }
        #[cfg(target_os = "windows")]
        if !matches!(
            request.target,
            CaptureTarget::Display(_)
                | CaptureTarget::Region(_)
                | CaptureTarget::Window(_)
                | CaptureTarget::Game(_)
        ) {
            return Err(unsupported_error(
                "This Windows capture target is unsupported.".into(),
                "Choose a listed source.",
            ));
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        if !matches!(
            request.target,
            CaptureTarget::Display(_) | CaptureTarget::Region(_)
        ) {
            return Err(unsupported_error(
                "This platform does not offer window or game capture.".into(),
                "Choose a display or region.",
            ));
        }
        #[cfg(target_os = "macos")]
        if request.camera.is_some() {
            return Err(unsupported_error(
                "Camera overlay is unavailable on macOS.".into(),
                "Record without a camera overlay.",
            ));
        }
        #[cfg(target_os = "macos")]
        if !matches!(request.fps_mode, frame_contracts::FpsMode::AutoMatchSource) {
            return Err(unsupported_error(
                "Fixed frame admission is unavailable on macOS.".into(),
                "Use automatic source frame rate.",
            ));
        }
        Ok(())
    }
}

/// Ignore is a real exclusion: only the selected routes reach the native A/V owner.
fn selected_routes(request: &RecordingRequest) -> Vec<AudioRouteSelection> {
    request
        .audio_routes
        .iter()
        .filter(|selection| selection.route != AudioRoute::Ignore)
        .cloned()
        .collect()
}

fn apply_saved_routes(request: &mut RecordingRequest, saved: &BTreeMap<AudioSourceId, AudioRoute>) {
    for selection in &mut request.audio_routes {
        if let Some(route) = saved.get(&selection.source_id) {
            selection.route = *route;
        }
    }
}

/// Reserved persisted file stem for native automatic recording names.
pub const AUTO_FILE_STEM: &str = "__freym_auto__";

/// Selects a name without creating a file; the MKV writer retains `create_new`.
#[cfg(test)]
fn auto_output_path(directory: &Path, timestamp: &str) -> Result<PathBuf, RecorderError> {
    crate::recording_files::select_output(directory, &format!("FREYM_{timestamp}"), true)
}

/// Resolves one absolute MKV path for a new session.
fn output_path(request: &RecordingRequest) -> Result<PathBuf, RecorderError> {
    crate::recording_files::resolve_output(
        Path::new(&request.output.directory),
        &request.output.file_stem,
        chrono::Local::now(),
    )
}

/// The exact nominal encoder rate, recovered from the display's reported refresh rate.
///
/// The accepted encoder is initialized with an explicit positive rational. The contract
/// carries the display's current-mode rate as an `f64`, so the smallest rational with a
/// denominator up to [`MAX_RATE_DENOMINATOR`] that reproduces the reported value is
/// recovered here; the capture pace itself always comes from the source, and this value
/// is configuration, never a measured throughput. An unknown or unrecoverable rate is
/// reported: a nominal rate is never invented.
#[cfg(any(target_os = "windows", target_os = "macos", test))]
fn nominal_rate(refresh_rate_hz: Option<f64>) -> Result<(NonZeroU32, NonZeroU32), RecorderError> {
    let Some(reported) = refresh_rate_hz else {
        return Err(invalid_request_error(
            "The display reported no refresh rate, so the nominal encoder rate is unknown."
                .to_string(),
            "Choose a display whose refresh rate is reported, or select another display mode.",
        ));
    };
    if !reported.is_finite() || reported <= 0.0 {
        return Err(invalid_request_error(
            format!("The display reported an unusable refresh rate of {reported} Hz."),
            "Choose a display whose refresh rate is reported, or select another display mode.",
        ));
    }
    let mut best: Option<(u32, u32, f64)> = None;
    for denominator in 1..=MAX_RATE_DENOMINATOR {
        let numerator = (reported * f64::from(denominator)).round();
        if numerator < 1.0 || numerator > f64::from(u32::MAX) {
            continue;
        }
        let error = (numerator / f64::from(denominator) - reported).abs();
        if best.is_none_or(|(_, _, best_error)| error < best_error) {
            best = Some((numerator as u32, denominator, error));
        }
    }
    let Some((numerator, denominator, error)) = best else {
        return Err(invalid_request_error(
            format!("The reported refresh rate of {reported} Hz cannot be written as a rational."),
            "Choose a display whose refresh rate is reported, or select another display mode.",
        ));
    };
    if error > NOMINAL_RATE_TOLERANCE_HZ {
        return Err(invalid_request_error(
            format!(
                "The reported refresh rate of {reported} Hz is farther than {NOMINAL_RATE_TOLERANCE_HZ} Hz from any exact rational with a denominator up to {MAX_RATE_DENOMINATOR}, so the nominal encoder rate would be a guess."
            ),
            "Choose a display whose refresh rate is reported, or select another display mode.",
        ));
    }
    let divisor = greatest_common_divisor(numerator, denominator);
    let numerator = NonZeroU32::new(numerator / divisor)
        .ok_or_else(|| internal_error("The nominal rate numerator became zero.".to_string()))?;
    let denominator = NonZeroU32::new(denominator / divisor)
        .ok_or_else(|| internal_error("The nominal rate denominator became zero.".to_string()))?;
    Ok((numerator, denominator))
}

#[cfg(any(target_os = "windows", target_os = "macos", test))]
fn greatest_common_divisor(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        let next = left % right;
        left = right;
        right = next;
    }
    left.max(1)
}

// --------------------------------------------------------------- native seam

/// One bounded step of the native recording owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeStep {
    /// The session is live; more input may still arrive.
    Recording,
    /// The encoder acknowledged the end of stream and every packet was written.
    Drained,
}

/// The real timing of one session: its own clock, its own origin and the rates the
/// accepted owner was configured with.
///
/// The clock is the one the capture session, the encoder and the container writer already
/// share, so `clock.now() - origin` is this recording's real elapsed wall time on its own
/// timeline. The container's media duration is a different observation and is only ever
/// read from the finalized artifact.
struct SessionTiming {
    clock: Arc<MasterClock>,
    origin: MediaTimestamp,
    /// The rate this source-paced request resolves to: the display's own rate.
    requested_fps: Option<f64>,
    /// The display's enumerated current-mode rate, the accepted source rate.
    source_rate_hz: Option<f64>,
}

/// The real cumulative counters and queue gauges the accepted video owner exposes.
///
/// They are counts, not rates: a rate is only ever computed by [`RateSampler`] over a real
/// elapsed window, so a session younger than one second reports no measured FPS.
#[derive(Clone, Debug, Default, PartialEq)]
struct NativeCounters {
    /// Completed capture surfaces, when the owner exposes a separate input count.
    capture_frames: Option<u64>,
    source_frames: Option<u64>,
    submitted_frames: Option<u64>,
    pipeline: Option<frame_contracts::VideoFrameCounters>,
    capture_queue: Option<(u32, u32, u64)>,
    /// Native pictures completed by the encoder, distinct from accepted inputs.
    completed_frames: Option<u64>,
    /// Live encoder queue gauges, absent when this owner exposes no encoder stats.
    encoder_queues: Option<EncoderQueueGauges>,
}

/// The two bounded queue gauges already published by `EncoderSessionStats`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EncoderQueueGauges {
    ingress_depth: u32,
    ingress_capacity: u32,
    packet_depth: u32,
    packet_capacity: u32,
}

impl EncoderQueueGauges {
    #[cfg(target_os = "windows")]
    fn from_encoder(stats: frame_windows::EncoderSessionStats) -> Self {
        // Encoder ingress is bounded by MAX_ENCODER_LEASES (3), and queued inputs are a
        // subset of those retained leases. Packet admission succeeds only below
        // MAX_PACKETS (4), and packets_held increments only after that bounded push. The
        // matching capacities are those same constants, so all four values fit u32.
        Self {
            ingress_depth: u32::try_from(stats.queued_inputs)
                .expect("the bounded encoder ingress depth fits QueueMetrics"),
            ingress_capacity: u32::try_from(stats.ingress_capacity)
                .expect("the bounded encoder ingress capacity fits QueueMetrics"),
            packet_depth: u32::try_from(stats.packets_held)
                .expect("the bounded encoder packet depth fits QueueMetrics"),
            packet_capacity: u32::try_from(stats.packet_capacity)
                .expect("the bounded encoder packet capacity fits QueueMetrics"),
        }
    }

    fn public_metrics(self) -> Vec<QueueMetrics> {
        vec![
            QueueMetrics {
                name: "Encoder ingress".to_string(),
                depth: self.ingress_depth,
                capacity: self.ingress_capacity,
                dropped: 0,
            },
            QueueMetrics {
                name: "Encoded packet delivery".to_string(),
                depth: self.packet_depth,
                capacity: self.packet_capacity,
                dropped: 0,
            },
        ]
    }
}

/// The Windows native owner, including its optional audio path.
#[cfg(target_os = "windows")]
enum NativeRecording {
    Silent(Box<crate::video_recording::VideoRecording>),
    Audio(Box<crate::av_recording::AvRecording>),
    #[cfg(test)]
    Scripted(Box<tests::Fix002Owner>),
}

#[cfg(target_os = "windows")]
impl NativeRecording {
    fn encoder_settings(&self) -> Option<&frame_windows::NativeEncoderSettings> {
        match self {
            Self::Silent(value) => value.encoder_settings(),
            Self::Audio(value) => value.encoder_settings(),
            #[cfg(test)]
            Self::Scripted(_) => None,
        }
    }
    fn request_stop(&mut self) -> Result<(), RecorderError> {
        match self {
            Self::Silent(value) => value.request_stop(),
            Self::Audio(value) => value.request_stop(),
            #[cfg(test)]
            Self::Scripted(value) => value.request_stop(),
        }
    }
    fn finish(&mut self) -> Result<RecordingArtifact, RecorderError> {
        match self {
            Self::Silent(value) => value.finish(),
            Self::Audio(value) => value.finish(),
            #[cfg(test)]
            Self::Scripted(_) => panic!("a scripted startup test must not finalize a recording"),
        }
    }
    fn abort_preparation(&mut self) -> Result<(), RecorderError> {
        match self {
            Self::Silent(value) => value.abort_preparation(),
            Self::Audio(value) => value.abort_preparation(),
            #[cfg(test)]
            Self::Scripted(value) => {
                crate::video_recording::preparation_encoder_result(value.abort())
            }
        }
    }

    fn abort(&mut self) -> Result<(), RecorderError> {
        match self {
            Self::Silent(value) => value.abort(),
            Self::Audio(value) => value.abort(),
            #[cfg(test)]
            Self::Scripted(value) => value.abort(),
        }
    }
    fn configure_replay(
        &mut self,
        limit: frame_mux::replay::ReplayLimit,
    ) -> Result<(), RecorderError> {
        match self {
            Self::Silent(value) => value.configure_replay(limit),
            Self::Audio(value) => value.configure_replay(limit),
            #[cfg(test)]
            Self::Scripted(value) => value.configure_replay(),
        }
    }
    fn replay_buffer(&self) -> Option<&frame_mux::replay::ReplayBuffer> {
        match self {
            Self::Silent(value) => Some(value.replay_buffer()),
            Self::Audio(value) => Some(value.replay_buffer()),
            #[cfg(test)]
            Self::Scripted(_) => None,
        }
    }
    fn enable_replay_only(&mut self) -> Result<(), RecorderError> {
        match self {
            Self::Silent(value) => value.enable_replay_only(),
            Self::Audio(value) => value.enable_replay_only(),
            #[cfg(test)]
            Self::Scripted(_) => Ok(()),
        }
    }
    fn replay_metadata_path(&self) -> Option<PathBuf> {
        match self {
            Self::Silent(value) => value.replay_metadata_path(),
            Self::Audio(value) => value.replay_metadata_path(),
            #[cfg(test)]
            Self::Scripted(_) => None,
        }
    }
    fn rolling_metadata(&self) -> Option<frame_metadata::RollingMetadata> {
        match self {
            Self::Silent(value) => value.rolling_metadata(),
            Self::Audio(value) => value.rolling_metadata(),
            #[cfg(test)]
            Self::Scripted(_) => None,
        }
    }
    fn pause(&mut self) -> Result<(), RecorderError> {
        match self {
            Self::Silent(value) => value.pause(),
            Self::Audio(value) => value.pause(),
            #[cfg(test)]
            Self::Scripted(_) => Ok(()),
        }
    }
    fn resume(&mut self) -> Result<(), RecorderError> {
        match self {
            Self::Silent(value) => value.resume(),
            Self::Audio(value) => value.resume(),
            #[cfg(test)]
            Self::Scripted(_) => Ok(()),
        }
    }
    fn active_timestamp(&self, raw: MediaTimestamp) -> MediaTimestamp {
        match self {
            Self::Silent(value) => value.active_timestamp(raw),
            Self::Audio(value) => value.active_timestamp(raw),
            #[cfg(test)]
            Self::Scripted(_) => raw,
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn validate_replay_settings(settings: ReplaySettings) -> Result<(), RecorderError> {
    if !(1..=600).contains(&settings.duration_seconds)
        || !(1024 * 1024..=512 * 1024 * 1024).contains(&settings.max_bytes)
        || !(16..=16384).contains(&settings.storage_mib)
    {
        return Err(invalid_request_error(
            "Replay duration must be 1–600 seconds, memory 1–512 MiB and storage 16–16384 MiB."
                .into(),
            "Choose supported replay limits.",
        ));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn replay_limit(settings: ReplaySettings) -> Result<frame_mux::replay::ReplayLimit, RecorderError> {
    validate_replay_settings(settings)?;
    Ok(frame_mux::replay::ReplayLimit {
        duration_ns: u64::from(settings.duration_seconds) * 1_000_000_000,
        max_bytes: settings.max_bytes as usize,
    })
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
fn replay_limit(settings: ReplaySettings) -> Result<(), RecorderError> {
    validate_replay_settings(settings)
}

#[cfg(target_os = "windows")]
type NativeStart = (
    NativeRecording,
    SessionTiming,
    SessionPlan,
    Vec<String>,
    Option<Arc<frame_windows::preview::PreviewHub>>,
);

#[cfg(not(target_os = "windows"))]
type NativeStart = (
    NativeRecording,
    SessionTiming,
    SessionPlan,
    Vec<String>,
    Option<()>,
);

#[cfg(target_os = "windows")]
const PREPARATION_CANCELLED: &str = "Recording preparation was cancelled before readiness.";

#[cfg(target_os = "windows")]
pub(crate) fn check_preparation_cancelled(cancel: &AtomicBool) -> Result<(), RecorderError> {
    if cancel.load(Ordering::Acquire) {
        Err(RecorderError {
            code: ErrorCode::InvalidTransition,
            message: PREPARATION_CANCELLED.into(),
            recovery: RecoveryStatus::NoRecording,
            action: "Start again when ready. An already-created partial output is not finalized."
                .into(),
        })
    } else {
        Ok(())
    }
}

#[cfg(all(test, target_os = "windows"))]
type PreparationSeam = Box<dyn FnMut(&AtomicBool) -> Result<NativeStart, RecorderError> + Send>;

#[cfg(target_os = "windows")]
pub(crate) fn windows_policy_candidates(
    request: &RecordingRequest,
    capture_adapter: &frame_windows::NativeAdapterIdentity,
    dimensions: Dimensions,
    rate_num: NonZeroU32,
    rate_den: NonZeroU32,
    hdr_main10: bool,
) -> Result<Vec<EncoderCandidate>, RecorderError> {
    let av1 = matches!(&request.encoder, frame_contracts::EncoderPreference::Explicit(id)
        if id.starts_with("nvidia-nvenc-av1-d3d11:") || id.starts_with("nvidia-nvenc-av1-main10-d3d11:")
            || id.starts_with("intel-mf-av1-d3d11:") || id.starts_with("amd-mf-av1-d3d11:") || id.starts_with("nvidia-mf-av1-d3d11:"));
    let mf_av1 = matches!(&request.encoder, frame_contracts::EncoderPreference::Explicit(id)
        if id.contains("-mf-av1-d3d11:"));
    let mut primary_error = None;
    let mut policy_candidates: Vec<EncoderCandidate> = if av1 {
        let profile = if hdr_main10 {
            frame_windows::NativeVideoProfile::Av1Main10
        } else {
            frame_windows::NativeVideoProfile::Av1Main8
        };
        let config = frame_windows::NativeEncoderConfig::proof(
            dimensions.width,
            dimensions.height,
            rate_num,
            rate_den,
        )?;
        let candidate = if mf_av1 {
            frame_windows::probe_mf_av1_for_adapter(capture_adapter, &config)?
        } else {
            frame_windows::probe_native_profile_for_adapter(capture_adapter, &config, profile)?
        };
        let support = candidate.settings.rate_control_support();
        vec![EncoderCandidate {
            id: candidate.capability.id,
            codec: "av1".into(),
            hardware: true,
            limits: EncoderLimits {
                max_width: candidate.settings.caps_width_max,
                max_height: candidate.settings.caps_height_max,
                cbr: support.cbr,
                vbr: support.vbr,
            },
        }]
    } else if hdr_main10 {
        let candidates = frame_windows::probe_native_hevc_main10().unwrap_or_else(|error| {
            primary_error = Some(error);
            Vec::new()
        });
        candidates
            .iter()
            .filter(|candidate| candidate.adapter.luid == capture_adapter.luid)
            .map(|candidate| {
                let support = candidate.settings.rate_control_support();
                EncoderCandidate {
                    id: format!(
                        "nvidia-nvenc-hevc-main10-d3d11:luid={:016X}",
                        candidate.adapter.luid
                    ),
                    codec: "hevc".into(),
                    hardware: true,
                    limits: EncoderLimits {
                        max_width: candidate.settings.caps_width_max,
                        max_height: candidate.settings.caps_height_max,
                        cbr: support.cbr,
                        vbr: support.vbr,
                    },
                }
            })
            .collect()
    } else {
        let candidates = frame_windows::probe_h264_encoder_for_adapter(
            capture_adapter,
            (dimensions.width, dimensions.height),
            (rate_num, rate_den),
        )
        .unwrap_or_else(|error| {
            primary_error = Some(error);
            Vec::new()
        });
        candidates
            .iter()
            .map(|candidate| {
                let support = candidate.settings.rate_control_support();
                EncoderCandidate {
                    id: candidate.capability.id.clone(),
                    codec: candidate.capability.codec.clone(),
                    hardware: candidate.capability.hardware,
                    limits: EncoderLimits {
                        max_width: candidate.settings.caps_width_max,
                        max_height: candidate.settings.caps_height_max,
                        cbr: support.cbr,
                        vbr: support.vbr,
                    },
                }
            })
            .collect()
    };
    if request.optimization.as_ref().is_some_and(|o| {
        o.codec != frame_contracts::RecordingCodec::H264
            && o.preset != frame_contracts::RecordingPreset::Lossless
    }) && request.encoder == frame_contracts::EncoderPreference::Auto
        && !av1
    {
        let config = frame_windows::NativeEncoderConfig::proof(
            dimensions.width,
            dimensions.height,
            rate_num,
            rate_den,
        )?;
        let optional = if capture_adapter.vendor_id == 0x10de {
            frame_windows::probe_native_profile_for_adapter(
                capture_adapter,
                &config,
                if hdr_main10 {
                    frame_windows::NativeVideoProfile::Av1Main10
                } else {
                    frame_windows::NativeVideoProfile::Av1Main8
                },
            )
        } else if !hdr_main10 {
            frame_windows::probe_mf_av1_for_adapter(capture_adapter, &config)
        } else {
            return Ok(policy_candidates);
        };
        if let Ok(candidate) = optional {
            let support = candidate.settings.rate_control_support();
            policy_candidates.push(EncoderCandidate {
                id: candidate.capability.id,
                codec: candidate.capability.codec,
                hardware: true,
                limits: EncoderLimits {
                    max_width: candidate.settings.caps_width_max,
                    max_height: candidate.settings.caps_height_max,
                    cbr: support.cbr,
                    vbr: support.vbr,
                },
            });
        }
    }
    if policy_candidates.is_empty()
        && let Some(error) = primary_error
    {
        return Err(error);
    }
    Ok(policy_candidates)
}

#[cfg(target_os = "windows")]
fn start_native(
    request: &RecordingRequest,
    output: &Path,
    meter: Arc<MeterHub>,
    meter_session: String,
    preview_quality: PreviewQuality,
    cancel_start: &AtomicBool,
) -> Result<NativeStart, RecorderError> {
    use frame_capture::DisplayBackend as _;

    check_preparation_cancelled(cancel_start)?;
    let window_id = match &request.target {
        CaptureTarget::Window(id) => Some(id.clone()),
        CaptureTarget::Game(game) => Some(frame_windows::resolve_game_window(game)?),
        _ => None,
    };
    check_preparation_cancelled(cancel_start)?;
    let window_source = window_id
        .as_ref()
        .map(frame_windows::window_recording_source)
        .transpose()?;
    let window_id = window_source
        .as_ref()
        .map(|source| source.window_id.clone());
    // The encoder configuration is the display's own: its enumerated geometry is the
    // native resolution and its enumerated refresh rate is the nominal rate, so nothing
    // is guessed and nothing is re-scaled. Enumeration runs on this control thread, never
    // on the interface thread.
    check_preparation_cancelled(cancel_start)?;
    let displays = frame_windows::PlatformBackend.enumerate_displays()?;
    let display_id = match &request.target {
        CaptureTarget::Display(id) => id.clone(),
        CaptureTarget::Region(region) => region.display_id.clone(),
        CaptureTarget::Window(_) | CaptureTarget::Game(_) => displays
            .iter()
            .find(|display| display.is_primary)
            .ok_or_else(|| {
                unsupported_error(
                    "Window recording needs an attached desktop display for its control session."
                        .into(),
                    "Attach a display and retry.",
                )
            })?
            .id
            .clone(),
    };
    let display = displays
        .iter()
        .find(|display| display.id == display_id)
        .ok_or_else(|| RecorderError {
            code: ErrorCode::SourceLost,
            message: format!(
                "The requested display {} is not attached, so no recording was prepared.",
                display_id.0
            ),
            recovery: RecoveryStatus::NoRecording,
            action: "Refresh the sources and choose an attached display.".to_string(),
        })?;
    let hdr_main10 = if window_source.is_some() {
        false
    } else {
        frame_windows::active_hdr_display(&display_id)? == Some(true)
    };
    if hdr_main10
        && (matches!(&request.target, CaptureTarget::Region(_)) || request.camera.is_some())
    {
        return Err(RecorderError {
            code: ErrorCode::NotImplemented,
            message: "HDR Main10 currently supports whole-display capture without region or camera composition; those paths accept BGRA8 only.".into(),
            recovery: RecoveryStatus::NoRecording,
            action: "Choose whole-display HDR capture without a camera, or use those features on an SDR display.".into(),
        });
    }
    crate::video_recording::validate_encoder_signal(
        hdr_main10,
        match &request.encoder {
            frame_contracts::EncoderPreference::Explicit(id) => Some(id.as_str()),
            _ => None,
        },
    )?;
    if !hdr_main10
        && matches!(&request.encoder, frame_contracts::EncoderPreference::Explicit(id) if id.starts_with("nvidia-nvenc-hevc-main10"))
    {
        return Err(RecorderError {
            code: ErrorCode::EncoderUnavailable,
            message: "HEVC Main10 was requested for a display without an active HDR signal; no FP16 scRGB source is available.".into(),
            recovery: RecoveryStatus::NoRecording,
            action: "Select an active HDR display for HEVC Main10.".into(),
        });
    }
    // WGC has no source refresh metadata. Use the selected display mode as
    // the nominal encoder budget, never as a measured window presentation rate.
    let (rate_num, rate_den) = if window_source.is_some() {
        match request.fps_mode {
            FpsMode::Fixed(fps) => {
                let rate = NominalRate::recover_exact(fps)?;
                (rate.num(), rate.den())
            }
            FpsMode::AutoMatchSource => nominal_rate(display.refresh_rate_hz)?,
        }
    } else {
        nominal_rate(display.refresh_rate_hz)?
    };
    // `nominal_rate` already refused an unknown or unusable rate, so this is the same
    // reported value it recovered the rational from.
    let source_rate_hz = if window_source.is_some() {
        None
    } else {
        Some(display.refresh_rate_hz.ok_or_else(|| {
            internal_error(
                "The display's refresh rate disappeared after it was validated.".to_string(),
            )
        })?)
    };
    let dimensions = if let Some(window) = &window_source {
        Dimensions {
            width: window.width,
            height: window.height,
        }
    } else {
        match &request.target {
            CaptureTarget::Region(region) => {
                let bounds = validate_region(region, display)?;
                Dimensions {
                    width: bounds.width(),
                    height: bounds.height(),
                }
            }
            _ => Dimensions {
                width: display.width,
                height: display.height,
            },
        }
    };
    check_preparation_cancelled(cancel_start)?;
    let capture_adapter = if let Some(window) = &window_source {
        if window.adapter.vendor_id != 0x10DE {
            return Err(unsupported_error(
                "The isolated WGC path currently requires NVENC on its capture adapter.".into(),
                "Use a supported NVIDIA adapter or choose display recording.",
            ));
        }
        window.adapter.clone()
    } else {
        frame_windows::display_capture_adapter_identity(&display_id)?
    };
    check_preparation_cancelled(cancel_start)?;
    let policy_candidates = windows_policy_candidates(
        request,
        &capture_adapter,
        dimensions,
        rate_num,
        rate_den,
        hdr_main10,
    )?;
    let inputs = PlanInputs {
        quality: request.quality,
        resolution: request.resolution,
        fps_mode: request.fps_mode,
        encoder: request.encoder.clone(),
        native: dimensions,
        nominal_rate: NominalRate::new(rate_num, rate_den),
        candidates: &policy_candidates,
    };
    let default_codec = if matches!(&request.encoder, frame_contracts::EncoderPreference::Explicit(id) if id.contains("av1"))
    {
        "av1"
    } else if hdr_main10 {
        "hevc"
    } else {
        "h264"
    };
    let assessments = crate::encoder_calibration::assessments(
        &capture_adapter,
        &inputs,
        request.optimization.as_ref(),
        default_codec,
    );
    check_preparation_cancelled(cancel_start)?;
    let plan = SessionPlan::freeze(frame_settings::policy::plan_optimized_recording(
        &inputs,
        request.optimization.as_ref(),
        default_codec,
        &assessments,
    )?);
    let codec = plan.plan().encoder.codec.clone();
    let policy = plan.plan();
    frame_settings::validation::validate_applied_plan(request, policy)?;
    if frame_settings::policy::windows_adapter_required_quality(capture_adapter.vendor_id)
        == Some(frame_contracts::Quality::Standard)
        && (policy.rate_control != RateControl::Cbr || policy.preset != EncoderPreset::LowLatency)
    {
        return Err(RecorderError {
            code: ErrorCode::EncoderUnavailable,
            message: format!(
                "The selected MF H.264 encoder on {} supports the verified CBR/low-latency configuration; this quality tier requests a different encoder preset.",
                capture_adapter.description
            ),
            recovery: RecoveryStatus::NoRecording,
            action: "Choose Standard quality for this Intel/AMD hardware encoder.".into(),
        });
    }
    let config = crate::encoder_calibration::config_for_plan(dimensions, policy)?;
    crate::encoder_calibration::track_recording(&capture_adapter, policy);
    let admission = match policy.frame_rate {
        FrameRatePlan::SourcePaced => frame_encode::admission::FrameAdmitter::auto(),
        FrameRatePlan::Fixed(rate) => {
            frame_encode::admission::FrameAdmitter::fixed(rate.num(), rate.den())
        }
    };
    // One shared session clock and one shared origin for this recording, handed to the
    // accepted owner so every container timestamp keeps its real position on the session
    // timeline. The same clock is what the elapsed telemetry is read from.
    let clock = Arc::new(frame_clock::MasterClock::default());
    let origin = clock.now();
    let region = match &request.target {
        CaptureTarget::Region(value) => Some(value.clone()),
        _ => None,
    };
    let routes = selected_routes(request);
    check_preparation_cancelled(cancel_start)?;
    let preview = if !hdr_main10 {
        Some(frame_windows::preview::new_hub(
            preview_quality.config(),
            PREVIEW_GENERATION.fetch_add(1, Ordering::Relaxed),
        )?)
    } else {
        None
    };
    check_preparation_cancelled(cancel_start)?;
    if let Some(hub) = &preview {
        hub.set_recording();
    }
    let recording = if routes.is_empty() {
        NativeRecording::Silent(Box::new(
            crate::video_recording::VideoRecording::start_with_preview_cancellable(
                crate::video_recording::VideoRecordingRequest {
                    hdr_main10,
                    display_id: display_id.clone(),
                    window_id: window_id.clone(),
                    config,
                    encoder_id: Some(policy.encoder.id.clone()),
                    clock: Arc::clone(&clock),
                    origin,
                    output: output.to_path_buf(),
                    camera: request.camera.clone(),
                    show_cursor: request.show_cursor,
                },
                region,
                admission,
                preview.as_ref(),
                cancel_start,
            )?,
        ))
    } else {
        check_preparation_cancelled(cancel_start)?;
        let sources =
            frame_audio::AudioBackend::enumerate_audio_sources(&frame_windows::PlatformBackend)?;
        frame_settings::audio_admission::validate_audio_selection(
            &routes,
            &sources,
            frame_windows::windows_build_number()? >= 20348,
            true,
        )?;
        for route in &routes {
            if !sources.iter().any(|source| source.id == route.source_id) {
                return Err(RecorderError {
                    code: ErrorCode::SourceLost,
                    message: format!(
                        "Selected audio source {} is no longer available.",
                        route.source_id.0
                    ),
                    recovery: RecoveryStatus::NoRecording,
                    action: "Refresh sources and choose an available source.".into(),
                });
            }
        }
        NativeRecording::Audio(Box::new(
            crate::av_recording::AvRecording::start_with_preview_cancellable(
                crate::av_recording::AvRecordingRequest {
                    hdr_main10,
                    display_id: display_id.clone(),
                    window_id: window_id.clone(),
                    config,
                    encoder_id: Some(policy.encoder.id.clone()),
                    routes: routes.clone(),
                    plan: frame_audio::plan_tracks(&routes)?,
                    clock: Arc::clone(&clock),
                    origin,
                    output: output.to_path_buf(),
                    meter: Arc::clone(&meter),
                    meter_session: meter_session.clone(),
                    camera: request.camera.clone(),
                    show_cursor: request.show_cursor,
                },
                region,
                admission,
                preview.as_ref(),
                cancel_start,
            )?,
        ))
    };
    let settings = recording.encoder_settings().ok_or_else(|| internal_error(
        "The prepared recording has no encoder readback, so its applied plan cannot be reported.".into()
    ))?;
    let mut readback = Vec::new();
    readback.push(format!(
        "Native accepted initialization: {}x{} at {}/{}, {} {} bit/s (peak {}), {}, GOP {} / IDR {}.",
        settings.width,
        settings.height,
        settings.rate_num,
        settings.rate_den,
        settings.rate_control.describe(),
        settings.bitrate_bps,
        settings.max_bitrate_bps,
        settings.preset_description(),
        settings.gop_length,
        settings.idr_period
    ));
    readback.push(format!("Native Advanced accepted fields: {:?}. Driver capabilities and codec sequence bytes are queried; rate-control/preset fields are accepted initialization snapshots, not a get-current-config query.", settings.advanced));
    if hdr_main10 {
        readback.push(format!("HDR capture: FP16 linear scRGB converted on the GPU to BT.2020/PQ RGB10, encoded as {} 10-bit. Region, camera and preview are unavailable for this format; mastering and content-light metadata were not supplied by the source.", codec.to_uppercase()));
    }
    if window_source.is_some() {
        readback.push(format!("Window capture: isolated WGC helper, SDR BGRA shared texture, adapter-matched NVENC {}. An odd right/bottom pixel is cropped for 4:2:0 encoding. Encoder nominal display-mode rate is not a measured window source rate; preview shares the capture slots and WGC cursor capture is {}.", codec.to_uppercase(), if request.show_cursor { "enabled" } else { "disabled" }));
    }
    readback.push(format!(
        "Audio: {} selected source(s), {} main/separate Opus track(s).",
        routes.len(),
        if routes.is_empty() {
            0
        } else {
            frame_audio::plan_tracks(&routes)?.len()
        }
    ));
    Ok((
        recording,
        SessionTiming {
            clock,
            origin,
            requested_fps: match policy.frame_rate {
                FrameRatePlan::Fixed(rate) => Some(rate.frames_per_second()),
                FrameRatePlan::SourcePaced => None,
            },
            source_rate_hz,
        },
        plan,
        readback,
        preview,
    ))
}

#[cfg(target_os = "linux")]
type NativeRecording = frame_linux::LinuxRecording;

#[cfg(target_os = "linux")]
fn start_native(
    request: &RecordingRequest,
    output: &Path,
    cancel_start: Arc<AtomicBool>,
) -> Result<NativeStart, RecorderError> {
    if !selected_routes(request).is_empty() {
        let audio = crate::audio_discovery::discover()?;
        frame_settings::audio_admission::validate_audio_selection(
            &request.audio_routes,
            &audio.sources,
            audio.per_app_audio,
            true,
        )?;
    }
    let recording = frame_linux::LinuxRecording::start(frame_linux::LinuxRecordingConfig {
        request: request.clone(),
        output: output.to_path_buf(),
        cancel_start,
    })?;
    let timing = SessionTiming {
        clock: recording.clock(),
        origin: recording.origin(),
        requested_fps: None,
        source_rate_hz: None,
    };
    let plan = recording.plan().clone();
    let readback = vec![
        "Linux video: portal-selected PipeWire screen, SHM and explicit CPU NV12 conversion, hardware VA-API H.264; no software fallback or DMABuf zero-copy.".into(),
        "Portal stream has no declared source refresh rate; the 30/1 value is encoder configuration only.".into(),
        format!("Audio: {} independently selected microphone/application source(s), common Opus tracks.", selected_routes(request).len()),
    ];
    Ok((recording, timing, plan, readback, None))
}

#[cfg(target_os = "linux")]
fn poll_native(recording: &mut NativeRecording) -> Result<NativeStep, RecorderError> {
    Ok(match recording.poll()? {
        frame_linux::RecordingProgress::Recording => NativeStep::Recording,
        frame_linux::RecordingProgress::Drained => NativeStep::Drained,
    })
}

#[cfg(target_os = "linux")]
fn native_counters(recording: &NativeRecording) -> NativeCounters {
    NativeCounters {
        // The fused PipeWire-to-VA pipeline exposes encoded output, not a
        // separate count of capture surfaces. Do not relabel that as capture FPS.
        capture_frames: None,
        completed_frames: Some(recording.completed_frames()),
        encoder_queues: None,
        ..NativeCounters::default()
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
#[derive(Debug)]
struct UnsupportedRecording;

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
type NativeRecording = UnsupportedRecording;

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn start_native(_request: &RecordingRequest, _output: &Path) -> Result<NativeStart, RecorderError> {
    Err(unsupported_error(
        "Native display recording is implemented for Windows only.".to_string(),
        "Use a Windows build to record a display.".to_string(),
    ))
}

#[cfg(target_os = "macos")]
type NativeRecording = crate::macos_recording::MacRecording;

#[cfg(target_os = "macos")]
fn start_native(request: &RecordingRequest, output: &Path) -> Result<NativeStart, RecorderError> {
    use frame_capture::DisplayBackend as _;
    let backend = frame_macos::PlatformBackend::default();
    let displays = backend.enumerate_displays()?;
    let display = match &request.target {
        CaptureTarget::Display(id) => displays.iter().find(|display| display.id == *id),
        CaptureTarget::Window(_) => displays.iter().find(|display| display.is_primary),
        _ => None,
    }
    .ok_or_else(|| RecorderError {
        code: ErrorCode::SourceLost,
        message: "Selected macOS capture target is no longer available.".into(),
        recovery: RecoveryStatus::NoRecording,
        action: "Refresh sources and select an available display or window.".into(),
    })?;
    let (width, height) = frame_macos::capture_dimensions(&request.target)?;
    let dimensions = Dimensions { width, height };
    // Variable-refresh displays may report no fixed rate. In that case 60 Hz is only
    // the declared encoder configuration rate, never a measured display/source rate.
    let source_rate_hz = if matches!(&request.target, CaptureTarget::Display(_)) {
        display
            .refresh_rate_hz
            .filter(|value| *value > 0.0 && value.is_finite())
    } else {
        None
    };
    let (rate_num, rate_den) = match source_rate_hz {
        Some(rate) => nominal_rate(Some(rate))?,
        None => (NonZeroU32::new(60).unwrap(), NonZeroU32::new(1).unwrap()),
    };
    let probe = frame_macos::probe_encoder(frame_macos::MacEncoderConfig {
        width,
        height,
        rate_num,
        rate_den,
        bitrate_bps: 1_000_000,
        peak_bitrate_bps: 2_000_000,
        gop_frames: 60,
    })?;
    let candidate = EncoderCandidate {
        id: probe.id,
        codec: probe.codec,
        hardware: probe.hardware,
        limits: EncoderLimits {
            max_width: width,
            max_height: height,
            cbr: false,
            vbr: true,
        },
    };
    let plan = SessionPlan::freeze(frame_settings::policy::plan_optimized_recording(
        &PlanInputs {
            quality: request.quality,
            resolution: request.resolution,
            fps_mode: request.fps_mode,
            encoder: request.encoder.clone(),
            native: dimensions,
            nominal_rate: NominalRate::new(rate_num, rate_den),
            candidates: &[candidate],
        },
        request.optimization.as_ref(),
        "h264",
        &[],
    )?);
    let policy = plan.plan();
    frame_settings::validation::validate_applied_plan(request, policy)?;
    let bitrate_bps = policy.bitrate_bps;
    let peak_bitrate_bps = policy.peak_bitrate_bps;
    let clock = Arc::new(MasterClock::default());
    let origin = clock.now();
    let routes = selected_routes(request);
    let sources = frame_audio::AudioBackend::enumerate_audio_sources(&backend)?;
    frame_settings::audio_admission::validate_audio_selection(&routes, &sources, true, true)?;
    for route in &routes {
        if !sources.iter().any(|source| source.id == route.source_id) {
            return Err(RecorderError {
                code: ErrorCode::SourceLost,
                message: format!(
                    "Selected audio source {} is no longer available.",
                    route.source_id.0
                ),
                recovery: RecoveryStatus::NoRecording,
                action: "Refresh sources and select an available audio source.".into(),
            });
        }
    }
    let recording = crate::macos_recording::MacRecording::start(
        request.target.clone(),
        frame_macos::MacEncoderConfig {
            width,
            height,
            rate_num: policy.nominal_rate.num(),
            rate_den: policy.nominal_rate.den(),
            bitrate_bps,
            peak_bitrate_bps,
            gop_frames: policy.gop_frames,
        },
        &routes,
        Arc::clone(&clock),
        origin,
        output,
    )?;
    let mut readback = vec![format!(
        "VideoToolbox H.264: {}x{} at {}/{}, average target {} bit/s; {} Opus track(s).",
        width,
        height,
        policy.nominal_rate.num(),
        policy.nominal_rate.den(),
        bitrate_bps,
        frame_audio::plan_tracks(&routes)?.len(),
    )];
    if source_rate_hz.is_none() {
        readback.push("This target has no reported fixed source rate; 60/1 is the encoder configuration rate, not a measured source rate.".into());
    }
    Ok((
        recording,
        SessionTiming {
            clock,
            origin,
            requested_fps: source_rate_hz,
            source_rate_hz,
        },
        plan,
        readback,
        None,
    ))
}

#[cfg(target_os = "macos")]
fn poll_native(recording: &mut NativeRecording) -> Result<NativeStep, RecorderError> {
    match recording.poll()? {
        crate::macos_recording::Phase::Recording => Ok(NativeStep::Recording),
        crate::macos_recording::Phase::Drained => Ok(NativeStep::Drained),
    }
}

#[cfg(target_os = "macos")]
fn native_counters(recording: &NativeRecording) -> NativeCounters {
    let (capture_frames, completed_frames) = recording.counts();
    NativeCounters {
        capture_frames: Some(capture_frames),
        completed_frames: Some(completed_frames),
        encoder_queues: None,
        ..NativeCounters::default()
    }
}

#[cfg(target_os = "windows")]
fn poll_native(recording: &mut NativeRecording) -> Result<NativeStep, RecorderError> {
    Ok(match recording {
        NativeRecording::Silent(value) => match value.poll()?.phase {
            crate::video_recording::VideoRecordingPhase::Drained => NativeStep::Drained,
            _ => NativeStep::Recording,
        },
        NativeRecording::Audio(value) => match value.poll()?.phase {
            crate::av_recording::AvRecordingPhase::Drained => NativeStep::Drained,
            _ => NativeStep::Recording,
        },
        #[cfg(test)]
        NativeRecording::Scripted(value) => value.poll(),
    })
}

#[cfg(target_os = "windows")]
fn native_counters(recording: &NativeRecording) -> NativeCounters {
    match recording {
        NativeRecording::Silent(value) => native_encoder_counters(
            value.stats().capture_frames,
            value.stats().accepted_frames,
            value.stats().packets_written,
            value.stats().backpressure_returns,
            value.encoder_stats(),
            value.capture_pipeline_stats(),
            value.admission_counters().frames_skipped_by_policy,
        ),
        NativeRecording::Audio(value) => native_encoder_counters(
            value.stats().capture_frames,
            value.stats().accepted_frames,
            value.stats().video_packets,
            value.stats().backpressure_returns,
            value.encoder_stats(),
            value.capture_pipeline_stats(),
            value.admission_counters().frames_skipped_by_policy,
        ),
        #[cfg(test)]
        NativeRecording::Scripted(_) => NativeCounters::default(),
    }
}

#[cfg(target_os = "windows")]
fn native_encoder_counters(
    capture_frames: u64,
    submitted_frames: u64,
    collected_frames: u64,
    backpressure_events: u64,
    stats: Option<frame_windows::EncoderSessionStats>,
    capture: Option<frame_capture::CapturePipelineStats>,
    policy_skips: u64,
) -> NativeCounters {
    NativeCounters {
        capture_frames: Some(capture.map_or(capture_frames, |c| c.captured_unique_frames)),
        source_frames: capture.map(|c| c.source_presentations),
        submitted_frames: Some(submitted_frames),
        completed_frames: stats
            .filter(|s| s.packets_match_frames())
            .map(|s| collected_frames.saturating_add(s.packets_held)),
        encoder_queues: stats.map(EncoderQueueGauges::from_encoder),
        capture_queue: capture.and_then(|c| {
            c.ready_depth
                .map(|depth| (depth, c.ready_capacity, c.ready_queue_drops))
        }),
        pipeline: capture
            .zip(stats)
            .map(|(c, e)| frame_contracts::VideoFrameCounters {
                source_presentations: c.source_presentations,
                captured_frames: c.captured_frames,
                captured_unique_frames: c.captured_unique_frames,
                submitted_frames,
                encoded_frames: collected_frames.saturating_add(e.packets_held),
                policy_skipped_frames: policy_skips,
                backpressure_events,
                pool_pressure_drops: c.pool_pressure_drops,
                ready_queue_drops: c.ready_queue_drops,
                contention_drops: c.contention_drops,
                upstream_accumulated_updates: c.upstream_accumulated_updates,
            }),
    }
}

/// The three teardown calls below exist on both platforms so the one control loop is
/// platform independent. On a non-Windows build no value of this type is ever
/// constructed — `start_native` refuses first — so these are unreachable and say so.
#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
impl UnsupportedRecording {
    fn active_timestamp(&self, raw: MediaTimestamp) -> MediaTimestamp {
        raw
    }
    fn pause(&mut self) -> Result<(), RecorderError> {
        Err(self.unreachable())
    }
    fn resume(&mut self) -> Result<(), RecorderError> {
        Err(self.unreachable())
    }
    fn poll(&mut self) -> Result<NativeStep, RecorderError> {
        Err(self.unreachable())
    }
    fn request_stop(&mut self) -> Result<(), RecorderError> {
        Err(self.unreachable())
    }
    fn finish(&mut self) -> Result<RecordingArtifact, RecorderError> {
        Err(self.unreachable())
    }
    fn abort(&mut self) -> Result<(), RecorderError> {
        Err(self.unreachable())
    }
    fn unreachable(&self) -> RecorderError {
        internal_error("A non-Windows build has no native recording owner to advance.".to_string())
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn poll_native(recording: &mut NativeRecording) -> Result<NativeStep, RecorderError> {
    recording.poll()
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn native_counters(_recording: &NativeRecording) -> NativeCounters {
    NativeCounters::default()
}

/// Whether one telemetry event is due under the 4 Hz bound.
///
/// The first call is always due, so a session publishes its elapsed immediately instead of
/// one interval later.
fn metrics_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|last| now.duration_since(last) >= METRICS_INTERVAL)
}

#[cfg(any(target_os = "windows", test))]
fn audio_clock_warning(
    health: &frame_audio::SourceClockHealth,
    compensation: Option<frame_audio::DriftCompensationHealth>,
) -> String {
    let correction = match compensation.filter(|state| state.enabled) {
        Some(state) => format!(
            "Automatic drift correction is enabled (applied {:.3} ppm; estimator {}).",
            state.applied_milli_ppm as f64 / 1_000.0,
            if state.reliable {
                "reliable"
            } else {
                "not yet reliable"
            },
        ),
        None => "Automatic drift correction is disabled.".into(),
    };
    format!(
        "Audio source {} has a measured clock-speed difference of {} ppm and a clock residual of {:.1} ms. {correction} This residual is not a measured content A/V offset.",
        health.source_id.0,
        health.drift.rate_ppm,
        health.drift.residual_ns as f64 / 1_000_000.0,
    )
}

/// Builds the one truthful telemetry value this host can publish.
///
/// Everything measured comes from the live session: `elapsed` is the session's own clock
/// and the two rates are real elapsed-window samples over the owner's own counters.
/// Encoder queue gauges are copied from the live owner's typed stats. Both bounded queues
/// return or fail rather than silently dropping an item, so their queue-local dropped count
/// is zero. Everything else this host does not measure — stage latencies, CPU, GPU, memory,
/// A/V offset, audio underruns, disk throughput and dropped frames — is `None`, never a
/// stand-in zero. `elapsed_ms` is active recording time with pauses removed; the finalized
/// artifact remains the authority for the container's exact duration.
fn session_metrics(
    elapsed: Duration,
    requested_fps: Option<f64>,
    source_rate_hz: Option<f64>,
    actual_capture_fps: Option<f64>,
    encoded_fps: Option<f64>,
    encoder_queues: Option<EncoderQueueGauges>,
) -> RecorderMetrics {
    RecorderMetrics {
        frame_rate: FrameRateStatus {
            requested_fps,
            source_rate_hz,
            actual_capture_fps,
            encoded_fps,
            dropped_frames: None,
            ..FrameRateStatus::default()
        },
        elapsed_ms: elapsed.as_secs_f64() * 1000.0,
        capture_latency_ms: None,
        encode_latency_ms: None,
        mux_latency_ms: None,
        queues: encoder_queues.map_or_else(Vec::new, EncoderQueueGauges::public_metrics),
        cpu_percent: None,
        gpu_percent: None,
        memory_bytes: None,
        av_offset_ms: None,
        audio_underruns: None,
        disk_bytes_per_second: None,
    }
}

// ------------------------------------------------------------------ control thread

/// One active session: the lifecycle identity, the native session's real timing and the
/// two per-stage rate samplers.
///
/// The output path belongs to the native owner that was created with it, so it is not
/// duplicated here. One session owns one clock and one set of samplers, so no measurement
/// can leak from a finished session into the next one.
struct ActiveSession {
    #[cfg(target_os = "windows")]
    optimizer_sample: Option<(u64, u64, u64)>,
    id: SessionId,
    #[cfg(target_os = "windows")]
    audio_clock_warning_sent: bool,
    audio_source_states: BTreeMap<AudioSourceId, frame_audio::SourceLifecycle>,
    /// Immutable encoder policy for the lifetime of this session.
    _plan: Option<SessionPlan>,
    /// `None` only between the published `preparing` state and the moment the native owner
    /// exists: a session without a native clock has nothing to measure and publishes no
    /// telemetry.
    timing: Option<SessionTiming>,
    /// Media elapsed at Stop, excluding both pause intervals and finalization work.
    frozen_elapsed: Option<Duration>,
    /// One sampler per stage, so a capture delivery can never be attributed to the encoder.
    capture_rate: RateSampler,
    source_rate: RateSampler,
    submitted_rate: RateSampler,
    source_fps: Option<f64>,
    submitted_fps: Option<f64>,
    encoded_rate: RateSampler,
    /// The last *completed* one-second rate sample of each stage, if any.
    ///
    /// A sample is a real measurement over its own window, so it stays reported until the
    /// next window completes instead of disappearing between windows. The value shown is
    /// always a measurement that was really made, never one invented for a tick that
    /// completed no window.
    capture_fps: Option<f64>,
    encoded_fps: Option<f64>,
}

impl ActiveSession {
    /// A session that has been accepted but owns no native clock yet.
    fn preparing(id: SessionId) -> Self {
        Self {
            #[cfg(target_os = "windows")]
            optimizer_sample: None,
            id,
            #[cfg(target_os = "windows")]
            audio_clock_warning_sent: false,
            audio_source_states: BTreeMap::new(),
            _plan: None,
            timing: None,
            frozen_elapsed: None,
            capture_rate: RateSampler::new(),
            source_rate: RateSampler::new(),
            submitted_rate: RateSampler::new(),
            source_fps: None,
            submitted_fps: None,
            encoded_rate: RateSampler::new(),
            capture_fps: None,
            encoded_fps: None,
        }
    }

    /// The same session once its native owner exists and the session clock is real.
    fn prepared(id: SessionId, timing: SessionTiming, plan: SessionPlan) -> Self {
        Self {
            #[cfg(target_os = "windows")]
            optimizer_sample: None,
            id,
            #[cfg(target_os = "windows")]
            audio_clock_warning_sent: false,
            audio_source_states: BTreeMap::new(),
            _plan: Some(plan),
            timing: Some(timing),
            frozen_elapsed: None,
            capture_rate: RateSampler::new(),
            source_rate: RateSampler::new(),
            submitted_rate: RateSampler::new(),
            source_fps: None,
            submitted_fps: None,
            encoded_rate: RateSampler::new(),
            capture_fps: None,
            encoded_fps: None,
        }
    }

    fn label(&self) -> String {
        self.id.to_string()
    }

    fn freeze_elapsed(&mut self, recording: &NativeRecording) {
        if let Some(timing) = &self.timing {
            self.frozen_elapsed = Some(Duration::from_nanos(
                recording
                    .active_timestamp(timing.clock.now())
                    .0
                    .saturating_sub(timing.origin.0),
            ));
        }
    }

    /// One measured telemetry value, or `None` while this session owns no session clock.
    ///
    /// The elapsed time comes from the live session clock — the same clock the capture
    /// session and the container use — and the two rates are real elapsed-window samples
    /// over the owner's own cumulative counters. Before one full second has passed no rate
    /// is reported at all, and the last completed window's rate is kept until the next one
    /// completes.
    fn metrics(&mut self, recording: &NativeRecording) -> Option<RecorderMetrics> {
        let timing = self.timing.as_ref()?;
        let elapsed = self.frozen_elapsed.unwrap_or_else(|| {
            Duration::from_nanos(
                recording
                    .active_timestamp(timing.clock.now())
                    .0
                    .saturating_sub(timing.origin.0),
            )
        });
        let counters = native_counters(recording);
        let sample_elapsed =
            Duration::from_nanos(timing.clock.now().0.saturating_sub(timing.origin.0));
        if let Some(sample) = counters
            .capture_frames
            .and_then(|count| self.capture_rate.observe(sample_elapsed, count))
        {
            self.capture_fps = Some(sample.rate_hz);
        }
        if let Some(sample) = counters
            .completed_frames
            .and_then(|count| self.encoded_rate.observe(sample_elapsed, count))
        {
            self.encoded_fps = Some(sample.rate_hz);
        }
        if let Some(sample) = counters
            .source_frames
            .and_then(|n| self.source_rate.observe(sample_elapsed, n))
        {
            self.source_fps = Some(sample.rate_hz);
        }
        if let Some(sample) = counters
            .submitted_frames
            .and_then(|n| self.submitted_rate.observe(sample_elapsed, n))
        {
            self.submitted_fps = Some(sample.rate_hz);
        }
        let mut metrics = session_metrics(
            elapsed,
            timing.requested_fps,
            timing.source_rate_hz,
            self.capture_fps,
            self.encoded_fps,
            counters.encoder_queues,
        );
        metrics.frame_rate.observed_source_fps = self.source_fps;
        metrics.frame_rate.submitted_fps = self.submitted_fps;
        if let Some(c) = &counters.pipeline {
            metrics.frame_rate.dropped_frames = Some(
                c.pool_pressure_drops
                    .saturating_add(c.ready_queue_drops)
                    .saturating_add(c.contention_drops),
            );
            #[cfg(target_os = "windows")]
            {
                let now = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
                let lost = c
                    .pool_pressure_drops
                    .saturating_add(c.ready_queue_drops)
                    .saturating_add(c.contention_drops);
                if let Some((since, frames, dropped)) = self.optimizer_sample {
                    if now.saturating_sub(since) >= 1_000_000_000 {
                        if let Some(plan) = &self._plan {
                            crate::encoder_calibration::observe_recording(
                                plan.plan(),
                                c.encoded_frames.saturating_sub(frames),
                                now.saturating_sub(since),
                                lost.saturating_sub(dropped),
                            );
                        }
                        self.optimizer_sample = Some((now, c.encoded_frames, lost));
                    }
                } else {
                    self.optimizer_sample = Some((now, c.encoded_frames, lost));
                }
            }
        }
        metrics.frame_rate.counters = counters.pipeline;
        if let Some((depth, capacity, dropped)) = counters.capture_queue {
            metrics.queues.insert(
                0,
                QueueMetrics {
                    name: "Capture ready".into(),
                    depth,
                    capacity,
                    dropped,
                },
            );
        }
        Some(metrics)
    }
}

/// The one thread that owns a recording.
struct ControlWorker {
    sink: Arc<dyn EngineEventSink>,
    shared: Arc<Mutex<PublishedState>>,
    shutdown: Arc<AtomicBool>,
    start_pending: Arc<AtomicBool>,
    command_gate: Arc<Mutex<()>>,
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    cancel_start: Arc<AtomicBool>,
    commands: Receiver<ControlCommand>,
    recorder: Recorder,
    session: Option<ActiveSession>,
    recording: Option<NativeRecording>,
    countdown: Option<PendingStart>,
    interaction_metadata: bool,
    #[cfg(all(test, target_os = "windows"))]
    preparation: Option<PreparationSeam>,
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    replay_settings: ReplaySettings,
    replay_only: bool,
    #[cfg(target_os = "windows")]
    replay_worker: Option<crate::replay_worker::ReplayWorker>,
    storage_monitor: Option<crate::recording_files::StorageMonitor>,
    storage_directory: Option<PathBuf>,
    /// When the last `Metrics` event was published, for the 4 Hz bound. Reset for every
    /// session, so a new recording publishes its elapsed immediately.
    last_metrics: Option<Instant>,
    #[cfg(target_os = "windows")]
    preview: Arc<Mutex<PreviewPublication>>,
    #[cfg(target_os = "windows")]
    idle_preview: Option<IdlePreview>,
    #[cfg(target_os = "windows")]
    preview_target: Option<CaptureTarget>,
    #[cfg(target_os = "windows")]
    preview_quality: PreviewQuality,
    #[cfg(target_os = "windows")]
    preview_show_cursor: bool,
    #[cfg(target_os = "windows")]
    preview_gpu_mode: bool,
    #[cfg(target_os = "windows")]
    meter: Arc<MeterHub>,
    #[cfg(target_os = "windows")]
    level_test: Option<crate::audio_levels::AudioLevelTest>,
}

struct PendingStart {
    request: RecordingRequest,
    output: PathBuf,
    deadline: Instant,
}

#[cfg(target_os = "windows")]
enum IdlePreviewCapture {
    Display(frame_windows::WindowsCaptureSession),
    Region(Box<frame_windows::RegionCaptureSession>),
    Window(Box<frame_windows::WindowsWindowCaptureSession>),
}

/// Capture-only owner on the recording control thread. It never opens an encoder,
/// microphone, muxer or output file.
#[cfg(target_os = "windows")]
struct IdlePreview {
    target: CaptureTarget,
    show_cursor: bool,
    hub: Arc<frame_windows::preview::PreviewHub>,
    capture: IdlePreviewCapture,
    // Bridge startup to the UI transport without losing the only static frame.
    bootstrap: Option<frame_windows::preview::PreviewSubscriber>,
    gpu_mode: bool,
}

#[cfg(target_os = "windows")]
impl IdlePreview {
    fn start(
        target: CaptureTarget,
        config: frame_windows::preview::PreviewConfig,
        show_cursor: bool,
        gpu_mode: bool,
    ) -> Result<Self, RecorderError> {
        let display_id = match &target {
            CaptureTarget::Display(id) => Some(id),
            CaptureTarget::Region(region) => Some(&region.display_id),
            CaptureTarget::Window(_) if gpu_mode => None,
            _ => {
                return Err(unsupported_error(
                    "Live preview currently supports a display or physical-pixel region.".into(),
                    "Select a display or region.",
                ));
            }
        };
        if let Some(id) = display_id
            && frame_windows::active_hdr_display(id)? == Some(true)
        {
            return Err(unsupported_error(
                "Live preview cannot convert an active HDR display to the SDR preview format."
                    .into(),
                "Use an SDR display for live preview.",
            ));
        }
        let hub = frame_windows::preview::new_hub(
            config,
            PREVIEW_GENERATION.fetch_add(1, Ordering::Relaxed),
        )?;
        let bootstrap = if gpu_mode {
            let sink = frame_windows::gpu_preview::GpuPreviewSink::new(hub.generation());
            hub.set_gpu_sink(Some(sink)).map_err(internal_error)?;
            None
        } else {
            Some(hub.subscribe())
        };
        let context = CaptureContext {
            clock: Arc::new(MasterClock::default()),
            cancellation: CaptureCancellation::default(),
        };
        let capture = match &target {
            CaptureTarget::Display(_) => {
                let session = frame_windows::WindowsCaptureSession::start_with_preview_hub_cursor(
                    &target,
                    context,
                    Arc::clone(&hub),
                    show_cursor,
                )?;
                IdlePreviewCapture::Display(session)
            }
            CaptureTarget::Region(region) => IdlePreviewCapture::Region(Box::new(
                frame_windows::RegionCaptureSession::start_with_preview_cursor(
                    region,
                    context,
                    Arc::clone(&hub),
                    show_cursor,
                )?,
            )),
            CaptureTarget::Window(id) => IdlePreviewCapture::Window(Box::new(
                frame_windows::WindowsWindowCaptureSession::start_selected_with_preview_cursor(
                    id,
                    frame_contracts::FpsMode::AutoMatchSource,
                    context,
                    Some(hub.clone()),
                    show_cursor,
                )?,
            )),
            _ => unreachable!("the target was checked above"),
        };
        Ok(Self {
            target,
            show_cursor,
            hub,
            capture,
            bootstrap,
            gpu_mode,
        })
    }

    fn poll(&mut self) -> Result<(), RecorderError> {
        if self.hub.subscriber_count() > 1 {
            self.bootstrap = None;
        }
        match &mut self.capture {
            IdlePreviewCapture::Display(session) => {
                let _ = session.try_frame()?;
            }
            IdlePreviewCapture::Region(session) => {
                let _ = session.try_frame()?;
            }
            IdlePreviewCapture::Window(session) => {
                let _ = session.try_frame()?;
            }
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<(), RecorderError> {
        self.hub.close();
        match &mut self.capture {
            IdlePreviewCapture::Display(session) => session.stop(),
            IdlePreviewCapture::Region(session) => session.stop(),
            IdlePreviewCapture::Window(session) => session.stop(),
        }
    }
}

impl ControlWorker {
    #[cfg(target_os = "windows")]
    fn stop_idle_preview(&mut self) -> Result<(), RecorderError> {
        if let Some(mut idle) = self.idle_preview.take() {
            self.clear_preview();
            idle.stop()
        } else {
            Ok(())
        }
    }

    #[cfg(target_os = "windows")]
    fn start_idle_preview(
        &mut self,
        target: CaptureTarget,
        show_cursor: bool,
        gpu_mode: bool,
    ) -> Result<(), RecorderError> {
        if self.idle_preview.as_ref().is_some_and(|current| {
            current.target == target
                && current.show_cursor == show_cursor
                && current.gpu_mode == gpu_mode
        }) {
            return Ok(());
        }
        self.stop_idle_preview()?;
        self.clear_preview();
        let idle = IdlePreview::start(
            target.clone(),
            self.preview_quality.config(),
            show_cursor,
            gpu_mode,
        )?;
        *self
            .preview
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = PreviewPublication {
            hub: Some(Arc::clone(&idle.hub)),
            error: None,
        };
        self.preview_target = Some(target);
        self.preview_show_cursor = show_cursor;
        self.preview_gpu_mode = gpu_mode;
        self.idle_preview = Some(idle);
        Ok(())
    }

    #[cfg(target_os = "windows")]
    fn open_preview(
        &mut self,
        target: CaptureTarget,
        quality: PreviewQuality,
        show_cursor: bool,
    ) -> Result<(), RecorderError> {
        if !matches!(self.recorder.state(), RecorderState::Idle)
            || self.start_pending.load(Ordering::Acquire)
        {
            return Err(invalid_transition_error(
                "Idle preview can be opened only while no recording is active.".into(),
            ));
        }
        if self.preview_quality != quality {
            self.preview_quality = quality;
            self.stop_idle_preview()?;
        }
        self.start_idle_preview(target, show_cursor, false)
    }

    #[cfg(target_os = "windows")]
    fn open_gpu_preview(
        &mut self,
        target: CaptureTarget,
        show_cursor: bool,
    ) -> Result<(), RecorderError> {
        if !matches!(
            target,
            CaptureTarget::Display(_) | CaptureTarget::Region(_) | CaptureTarget::Window(_)
        ) {
            return Err(invalid_request_error(
                "GPU preview supports displays and physical-pixel areas.".into(),
                "Select a display or area.",
            ));
        }
        if !matches!(self.recorder.state(), RecorderState::Idle)
            || self.start_pending.load(Ordering::Acquire)
        {
            return Err(invalid_transition_error(
                "Idle GPU preview requires an idle recorder.".into(),
            ));
        }
        self.preview_quality = PreviewQuality::Native;
        self.start_idle_preview(target, show_cursor, true)
    }

    #[cfg(target_os = "windows")]
    fn close_preview(&mut self) -> Result<(), RecorderError> {
        if self.recording.is_none() && self.countdown.is_none() {
            self.preview_target = None;
            let result = self.stop_idle_preview();
            self.clear_preview();
            result
        } else {
            Ok(())
        }
    }

    #[cfg(target_os = "windows")]
    fn restore_idle_preview(&mut self) {
        #[cfg(test)]
        if self.preparation.is_some() {
            return;
        }
        if let Some(target) = self.preview_target.clone()
            && let Err(error) =
                self.start_idle_preview(target, self.preview_show_cursor, self.preview_gpu_mode)
        {
            self.publish_preview_error(error);
        }
    }

    #[cfg(target_os = "windows")]
    fn publish_preview_error(&self, error: RecorderError) {
        let mut publication = self
            .preview
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(hub) = publication.hub.take() {
            hub.close();
        }
        publication.error = Some(error);
    }

    #[cfg(target_os = "windows")]
    fn start_levels(&mut self, ids: Vec<AudioSourceId>) -> Result<(), RecorderError> {
        if ids.is_empty() {
            return Err(invalid_request_error(
                "Select an audio source to test.".into(),
                "Choose at least one microphone or application source.",
            ));
        }
        if self.recording.is_some() {
            let current = self.meter.snapshot();
            let mut active: Vec<_> = current
                .sources
                .iter()
                .map(|source| source.source_id.clone())
                .collect();
            let mut requested = ids;
            active.sort();
            requested.sort();
            return if active == requested {
                Ok(())
            } else {
                Err(invalid_transition_error("The recording already owns its audio sources; select its current sources to read their levels.".into()))
            };
        }
        if !matches!(self.recorder.state(), RecorderState::Idle) {
            return Err(invalid_transition_error(
                "Audio level testing can start only while idle or on the active recording.".into(),
            ));
        }
        self.stop_levels()?;
        let label = format!("test-{}", LEVEL_GENERATION.fetch_add(1, Ordering::Relaxed));
        self.level_test = Some(crate::audio_levels::AudioLevelTest::start(
            &ids,
            Arc::clone(&self.meter),
            label,
        )?);
        Ok(())
    }

    #[cfg(target_os = "windows")]
    fn stop_levels(&mut self) -> Result<(), RecorderError> {
        if let Some(mut test) = self.level_test.take() {
            test.stop()
        } else {
            Ok(())
        }
    }

    #[cfg(target_os = "windows")]
    fn publish_level_error(&self, error: RecorderError) {
        let (sequence, session_id) = {
            let mut shared = self
                .shared
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            shared.sequence = shared.sequence.wrapping_add(1);
            (shared.sequence, shared.session_id.clone())
        };
        self.sink.publish(EventEnvelope {
            sequence,
            session_id,
            event: EngineEvent::Warning(EngineWarning {
                code: "audioLevelTestFailed".into(),
                message: error.message,
                action: error.action,
            }),
        });
    }

    /// Runs until the host shuts down, the command channel closes or the loop unwinds.
    ///
    /// Every step is bounded: the wait is either a real command or one poll interval, and
    /// one step advances the native owner by at most one of its own bounded polls.
    fn run(mut self) {
        #[cfg(target_os = "windows")]
        let mut media_scheduling = None;
        loop {
            if self.shutdown.load(Ordering::Acquire) {
                break;
            }
            let wait = if self.recording.is_some() || self.countdown.is_some() || {
                #[cfg(target_os = "windows")]
                {
                    self.level_test.is_some() || self.idle_preview.is_some()
                }
                #[cfg(not(target_os = "windows"))]
                {
                    false
                }
            } {
                LIVE_POLL_INTERVAL
            } else {
                IDLE_POLL_INTERVAL
            };
            // Notifications are coalesced tokens, not queued frame messages.
            // Check commands before parking; a concurrent send/unpark survives.
            let command = match self.commands.try_recv() {
                Ok(command) => Ok(command),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    #[cfg(target_os = "windows")]
                    let wait_started =
                        frame_windows::cursor_preview_diagnostics::enabled().then(Instant::now);
                    std::thread::park_timeout(wait);
                    #[cfg(target_os = "windows")]
                    if let Some(started) = wait_started {
                        frame_windows::cursor_preview_diagnostics::record(
                            "recording_owner_wait",
                            None,
                            || serde_json::json!({"requested_ns": wait.as_nanos(), "elapsed_ns": started.elapsed().as_nanos()}),
                        );
                    }
                    self.commands.try_recv().map_err(|error| match error {
                        std::sync::mpsc::TryRecvError::Empty => RecvTimeoutError::Timeout,
                        std::sync::mpsc::TryRecvError::Disconnected => {
                            RecvTimeoutError::Disconnected
                        }
                    })
                }
            };
            match command {
                Ok(command) => self.handle(command),
                Err(RecvTimeoutError::Timeout) => {}
                // The owner is gone, so nothing can be commanded or recorded any more.
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if self.shutdown.load(Ordering::Acquire) {
                break;
            }
            #[cfg(target_os = "windows")]
            if self.recording.is_some() {
                if media_scheduling.is_none() {
                    // Capture notifications cannot wake an ordinary-priority
                    // consumer promptly when Windows defers that thread. Give
                    // the bounded mux/admission owner the same media scheduling
                    // class as acquisition, only while it owns a recording.
                    // Some(None) retains an unavailable-policy result without
                    // retrying a failed native registration on every frame.
                    media_scheduling = Some(frame_windows::WindowsMediaScheduling::join());
                }
            } else {
                media_scheduling = None;
            }
            if self
                .countdown
                .as_ref()
                .is_some_and(|pending| Instant::now() >= pending.deadline)
            {
                let pending = self.countdown.take().expect("deadline was checked");
                self.prepare_native(pending.request, pending.output);
            }
            self.advance();
            self.poll_recording_storage();
            #[cfg(target_os = "windows")]
            if let (Some(recording), Some(worker)) =
                (self.recording.as_ref(), self.replay_worker.as_mut())
                && let Some(buffer) = recording.replay_buffer()
            {
                worker.checkpoint(buffer, recording.rolling_metadata());
            }
            #[cfg(target_os = "windows")]
            if let Some(worker) = self.replay_worker.as_ref() {
                self.publish_replay_results(worker.results());
            }

            #[cfg(target_os = "windows")]
            if let Some(idle) = self.idle_preview.as_mut()
                && let Err(error) = idle.poll()
            {
                let _ = self.stop_idle_preview();
                self.publish_preview_error(error);
            }
            #[cfg(target_os = "windows")]
            if let Some(test) = self.level_test.as_mut()
                && let Err(error) = test.poll()
            {
                let _ = self.stop_levels();
                self.publish_level_error(error);
            }
            // Telemetry is published from this one loop, under the 4 Hz bound, so it can
            // never pace the recording or grow a queue of its own.
            self.publish_metrics(false);
        }
        self.release_recording();
        #[cfg(target_os = "windows")]
        let _ = self.stop_idle_preview();
        #[cfg(target_os = "windows")]
        let _ = self.stop_levels();
    }

    fn handle(&mut self, command: ControlCommand) {
        match command {
            ControlCommand::ConfigureInteractionMetadata(enabled) => {
                self.interaction_metadata = enabled
            }
            ControlCommand::Start {
                request,
                output,
                countdown_seconds,
                replay_only,
            } => {
                if matches!(self.recorder.state(), RecorderState::Idle) {
                    self.replay_only = replay_only;
                }
                self.start(*request, output, countdown_seconds)
            }
            ControlCommand::Stop => self.stop(),
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            ControlCommand::Pause => self.pause(),
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            ControlCommand::Resume => self.resume(),
            #[cfg(target_os = "windows")]
            ControlCommand::OpenPreview {
                target,
                quality,
                show_cursor,
                reply,
            } => {
                let result = self.open_preview(target, quality, show_cursor);
                if reply.send(result).is_err() {
                    // A timed-out caller cannot leave a late idle capture behind.
                    let _ = self.close_preview();
                }
            }
            #[cfg(target_os = "windows")]
            ControlCommand::OpenGpuPreview {
                target,
                show_cursor,
                reply,
            } => {
                let result = self.open_gpu_preview(target, show_cursor);
                if reply.send(result).is_err() {
                    let _ = self.close_preview();
                }
            }
            #[cfg(target_os = "windows")]
            ControlCommand::ClosePreview { reply } => {
                let _ = reply.send(self.close_preview());
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            ControlCommand::ConfigureReplay(settings) => self.configure_replay(settings),
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            ControlCommand::SaveReplay {
                path,
                duration_seconds,
            } => self.save_replay(path, duration_seconds),
            ControlCommand::ReportError(error) => self.publish_command_error(error),
            ControlCommand::Reset => self.reset(),
            ControlCommand::UpdateRoute { source_id, route } => self.update_route(source_id, route),
            ControlCommand::UpdateMute { source_id, muted } => self.update_mute(source_id, muted),
            ControlCommand::UpdateAudioControls {
                source_id,
                controls,
            } => self.update_audio_controls(source_id, controls),
            #[cfg(target_os = "windows")]
            ControlCommand::StartLevels { ids, reply } => {
                let _ = reply.send(self.start_levels(ids));
            }
            #[cfg(target_os = "windows")]
            ControlCommand::StopLevels { reply } => {
                let _ = reply.send(self.stop_levels());
            }
            // The flag is the signal; this command only wakes a waiting loop.
            ControlCommand::Shutdown => {}
        }
    }

    /// Begins one session and prepares its native owner on this thread.
    ///
    /// The prepared state is published *before* the native preparation starts, so a caller
    /// can never see idle while a capture session is being created, and a second start is
    /// refused by [`RecordingControl::start`] instead of reaching this thread. Publishing
    /// `preparing` is also what clears the previous session's telemetry and artifact: a
    /// finished file stays visible until the next session really begins.
    fn start(&mut self, mut request: RecordingRequest, output: PathBuf, countdown_seconds: u8) {
        if !matches!(self.recorder.state(), RecorderState::Idle) {
            self.start_pending.store(false, Ordering::Release);
            // The caller's gate reports this; a command that still arrives is not allowed
            // to disturb the live session.
            return;
        }
        #[cfg(target_os = "windows")]
        {
            self.preview_target = Some(request.target.clone());
            self.preview_show_cursor = request.show_cursor;
        }
        #[cfg(target_os = "windows")]
        if let Err(error) = self.stop_levels() {
            self.reject(error);
            return;
        }
        // An UpdateRoute command ahead of Start in this bounded channel is authoritative
        // even if its event has not reached the window yet. A direct caller may still
        // supply a route for a source with no stored choice.
        {
            let shared = self
                .shared
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            apply_saved_routes(&mut request, &shared.saved_routes);
            for selection in &mut request.audio_routes {
                if let Some(controls) = shared.saved_audio_controls.get(&selection.source_id) {
                    selection.controls = *controls;
                }
                if let Some(muted) = shared.saved_mutes.get(&selection.source_id) {
                    selection.muted = *muted;
                }
            }
        }
        if let Err(error) = validate_scope(&request) {
            self.reject(error);
            return;
        }
        let session = match self.recorder.begin(&request) {
            Ok(session) => session,
            Err(error) => {
                self.reject(error);
                return;
            }
        };
        self.session = Some(ActiveSession::preparing(session));
        self.storage_directory = output.parent().map(Path::to_path_buf);
        #[cfg(target_os = "windows")]
        {
            let ids: Vec<AudioSourceId> = selected_routes(&request)
                .into_iter()
                .map(|route| route.source_id)
                .collect();
            self.meter.begin(session.to_string(), &ids);
        }
        self.last_metrics = None;
        self.publish_state(RecorderState::Preparing);
        if countdown_seconds > 0 {
            self.countdown = Some(PendingStart {
                request,
                output,
                deadline: Instant::now() + Duration::from_secs(u64::from(countdown_seconds)),
            });
            return;
        }
        let output = if self.replay_only {
            let directory = output.with_extension("freym-replay");
            if let Err(error) = std::fs::create_dir(&directory) {
                self.fail_session(invalid_output_error(format!(
                    "Could not create private replay folder: {error}"
                )));
                return;
            }
            directory.join("capture.mkv")
        } else {
            output
        };
        self.prepare_native(request, output);
    }

    #[cfg(target_os = "windows")]
    fn start_windows_native(
        &mut self,
        request: &RecordingRequest,
        output: &Path,
        session: SessionId,
    ) -> Result<NativeStart, RecorderError> {
        check_preparation_cancelled(&self.cancel_start)?;
        #[cfg(test)]
        if let Some(preparation) = self.preparation.as_mut() {
            return preparation(&self.cancel_start);
        }
        start_native(
            request,
            output,
            Arc::clone(&self.meter),
            session.to_string(),
            self.preview_quality,
            &self.cancel_start,
        )
    }

    /// Called only after every partial native owner has been released and joined.
    #[cfg(target_os = "windows")]
    fn cancelled_preparation(&mut self, session: SessionId, output: &Path) {
        self.clear_preview();
        self.meter.end();
        self.countdown = None;
        if let Err(error) = self
            .recorder
            .stop()
            .and_then(|_| self.recorder.finalized(session))
        {
            self.fail_session(error);
            return;
        }
        if output.exists() {
            let (sequence, session_id) = {
                let mut shared = self
                    .shared
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                shared.sequence = shared.sequence.wrapping_add(1);
                (shared.sequence, shared.session_id.clone())
            };
            self.sink.publish(EventEnvelope {
                sequence,
                session_id,
                event: EngineEvent::Warning(EngineWarning {
                    code: "preparationCancelledPartial".into(),
                    message: format!(
                        "Preparation was cancelled. {} remains an incomplete output.",
                        output.display()
                    ),
                    action: "Use explicit recovery if needed; no completed recording was produced."
                        .into(),
                }),
            });
        }
        self.session = None;
        self.publish_state(RecorderState::Idle);
    }

    fn prepare_native(&mut self, request: RecordingRequest, output: PathBuf) {
        let Some(session) = self.session.as_ref().map(|active| active.id) else {
            return;
        };
        #[cfg(target_os = "windows")]
        if check_preparation_cancelled(&self.cancel_start).is_err() {
            self.cancelled_preparation(session, &output);
            return;
        }
        // A countdown keeps the selected source visible. Join its capture-only
        // owner immediately before preparing the recording so the two owners can
        // never capture the same display concurrently.
        #[cfg(target_os = "windows")]
        if let Err(error) = self.stop_idle_preview() {
            self.fail_session(error);
            return;
        }
        let mut output = output;
        let mut collision_retries = 0;
        let started = loop {
            #[cfg(target_os = "windows")]
            let started = self.start_windows_native(&request, &output, session);
            #[cfg(not(any(target_os = "windows", target_os = "linux")))]
            let started = start_native(&request, &output);
            #[cfg(target_os = "linux")]
            let started = start_native(&request, &output, Arc::clone(&self.cancel_start));
            #[cfg(target_os = "windows")]
            if self.cancel_start.load(Ordering::Acquire) {
                break started;
            }
            if matches!(&started, Err(error) if (request.output.file_stem == AUTO_FILE_STEM || request.output.file_stem.contains('{'))
                && error.code == ErrorCode::InvalidOutputPath
                && error.recovery == RecoveryStatus::NoRecording
                && std::fs::symlink_metadata(&output).is_ok())
                && collision_retries < 2
            {
                collision_retries += 1;
                match output_path(&request) {
                    Ok(next) if next != output => {
                        output = next;
                        continue;
                    }
                    Ok(_) => {}
                    Err(error) => break Err(error),
                }
            }
            break started;
        };
        match started {
            Ok((recording, timing, plan, readback, preview)) => {
                #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
                let mut recording = recording;
                #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
                let recording = recording;
                #[cfg(target_os = "windows")]
                if check_preparation_cancelled(&self.cancel_start).is_err() {
                    let cleanup = recording.abort_preparation();
                    drop(recording);
                    match cleanup {
                        Ok(()) => self.cancelled_preparation(session, &output),
                        Err(mut error) => {
                            if output.exists() {
                                error.recovery = RecoveryStatus::Unknown;
                            }
                            self.fail_session(error);
                        }
                    }
                    return;
                }
                #[cfg(target_os = "windows")]
                let metadata_start_error = if self.interaction_metadata
                    && !crate::metadata::enabled()
                {
                    match &mut recording {
                        NativeRecording::Silent(value) => value
                            .enable_metadata_for_source(frame_metadata::MetadataOptions::default()),
                        NativeRecording::Audio(value) => value
                            .enable_metadata_for_source(frame_metadata::MetadataOptions::default()),
                        #[cfg(test)]
                        NativeRecording::Scripted(value) => value.enable_metadata(),
                    }
                    .err()
                } else {
                    None
                };
                #[cfg(target_os = "windows")]
                if let Err(error) = recording.configure_replay(
                    replay_limit(self.replay_settings).expect("validated replay settings"),
                ) {
                    self.fail_session(error);
                    return;
                }
                #[cfg(target_os = "windows")]
                if recording.replay_buffer().is_some() {
                    if self.replay_only
                        && let Err(error) = recording.enable_replay_only()
                    {
                        let _ = recording.abort_preparation();
                        self.fail_session(error);
                        return;
                    }
                    let directory = if self.replay_only {
                        output.parent().unwrap().join("buffer")
                    } else {
                        output.with_extension("freym-replay")
                    };
                    let segment_seconds = u64::from(self.replay_settings.duration_seconds)
                        .div_ceil(128)
                        .max(1);
                    match crate::replay_worker::ReplayWorker::start(
                        directory,
                        frame_mux::replay::DiskReplayLimit {
                            segment_ns: segment_seconds * 1_000_000_000,
                            history_ns: u64::from(self.replay_settings.duration_seconds)
                                * 1_000_000_000,
                            max_bytes: u64::from(self.replay_settings.storage_mib) * 1024 * 1024,
                            max_segments: 256,
                        },
                    ) {
                        Ok(worker) => self.replay_worker = Some(worker),
                        Err(error) => {
                            let _ = recording.abort_preparation();
                            self.fail_session(error);
                            return;
                        }
                    }
                }
                #[cfg(not(target_os = "windows"))]
                let _ = preview;
                let mut decisions: Vec<String> = plan
                    .plan()
                    .decisions
                    .iter()
                    .map(|decision| decision.detail.clone())
                    .collect();
                decisions.extend(readback);
                let gate = Arc::clone(&self.command_gate);
                let admission = gate.lock().unwrap_or_else(|poison| poison.into_inner());
                #[cfg(target_os = "windows")]
                if check_preparation_cancelled(&self.cancel_start).is_err() {
                    drop(admission);
                    let cleanup = recording.abort_preparation();
                    drop(recording);
                    match cleanup {
                        Ok(()) => self.cancelled_preparation(session, &output),
                        Err(mut error) => {
                            if output.exists() {
                                error.recovery = RecoveryStatus::Unknown;
                            }
                            self.fail_session(error);
                        }
                    }
                    return;
                }
                #[cfg(target_os = "windows")]
                {
                    *self
                        .preview
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner()) = PreviewPublication {
                        hub: preview,
                        error: None,
                    };
                }
                #[cfg(target_os = "windows")]
                let metadata_warning = metadata_start_error.map(|error| {
                    decisions.push(format!("Interaction metadata unavailable: {error}"));
                    let (sequence, session_id) = {
                        let mut shared = self.shared.lock().unwrap_or_else(|p| p.into_inner());
                        shared.sequence = shared.sequence.wrapping_add(1);
                        (shared.sequence, shared.session_id.clone())
                    };
                    EventEnvelope {
                        sequence,
                        session_id,
                        event: EngineEvent::Warning(EngineWarning {
                            code: "interactionMetadataUnavailable".into(),
                            message: format!(
                                "Interaction metadata could not start: {}",
                                error.message
                            ),
                            action: "The video recording continues. Studio can use manual zooms."
                                .into(),
                        }),
                    }
                });
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                if self.interaction_metadata
                    && !self.replay_only
                    && std::env::var_os("FREYM_RECORD_METADATA").is_none_or(|value| value != "1")
                    && let Err(error) =
                        recording.enable_metadata(frame_metadata::MetadataOptions::default())
                {
                    decisions.push(format!("Interaction metadata unavailable: {error}"));
                }
                self.recording = Some(recording);
                self.session = Some(ActiveSession::prepared(session, timing, plan));
                match self.recorder.ready(session) {
                    Ok(()) => {
                        let plan_event = self.store_plan(decisions);
                        let state_event = self.store_state(RecorderState::Recording);
                        drop(admission);
                        #[cfg(target_os = "windows")]
                        if let Some(warning) = metadata_warning {
                            self.sink.publish(warning);
                        }
                        self.sink.publish(plan_event);
                        self.sink.publish(state_event);
                        // The first telemetry is published immediately, so the elapsed
                        // time starts from the session's own clock instead of one
                        // telemetry interval later.
                        self.publish_metrics(true);
                    }
                    Err(error) => {
                        drop(admission);
                        #[cfg(target_os = "windows")]
                        if let Some(warning) = metadata_warning {
                            self.sink.publish(warning);
                        }
                        self.fail_recording(error);
                    }
                }
            }
            // The native error keeps its own code, message, recovery and action.
            Err(mut error) => {
                #[cfg(target_os = "windows")]
                if error.code == ErrorCode::InvalidTransition
                    && error.message == PREPARATION_CANCELLED
                {
                    self.cancelled_preparation(session, &output);
                    return;
                }
                #[cfg(target_os = "windows")]
                if self.cancel_start.load(Ordering::Acquire) && output.exists() {
                    error.recovery = RecoveryStatus::Unknown;
                }
                if request.output.file_stem == AUTO_FILE_STEM
                    && error.code == ErrorCode::InvalidOutputPath
                {
                    error.action =
                        "Choose an accessible recording folder in More Options and retry."
                            .to_string();
                }
                #[cfg(target_os = "linux")]
                if self.cancel_start.load(Ordering::Acquire) {
                    if let Err(error) = self
                        .recorder
                        .stop()
                        .and_then(|_| self.recorder.finalized(session))
                    {
                        self.fail_session(error);
                    } else {
                        self.session = None;
                        self.publish_state(RecorderState::Idle);
                    }
                    return;
                }
                self.fail_session(error)
            }
        }
    }

    fn update_audio_controls(&mut self, source_id: AudioSourceId, controls: AudioSourceControls) {
        if !matches!(self.recorder.state(), RecorderState::Idle) || controls.validate().is_err() {
            return;
        }
        let (sequence, sources) = {
            let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            let Some(source) = shared.audio_sources.iter_mut().find(|s| s.id == source_id) else {
                return;
            };
            source.controls = controls;
            shared.saved_audio_controls.insert(source_id, controls);
            shared.sequence = shared.sequence.wrapping_add(1);
            (shared.sequence, shared.audio_sources.clone())
        };
        self.sink.publish(EventEnvelope {
            sequence,
            session_id: None,
            event: EngineEvent::AudioSourcesChanged(sources),
        });
    }

    fn update_mute(&mut self, source_id: AudioSourceId, muted: bool) {
        if !matches!(
            self.recorder.state(),
            RecorderState::Idle | RecorderState::Recording | RecorderState::Paused
        ) {
            return;
        }
        if let Some(recording) = self.recording.as_mut() {
            #[cfg(target_os = "windows")]
            let result = match recording {
                NativeRecording::Audio(audio) => audio.set_audio_muted(&source_id, muted),
                NativeRecording::Silent(_) => Ok(()),
                #[cfg(test)]
                NativeRecording::Scripted(_) => Ok(()),
            };
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            let result = recording.set_audio_muted(&source_id, muted);
            #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
            let result: Result<(), RecorderError> = Ok(());
            if let Err(error) = result {
                self.publish_command_error(error);
                return;
            }
        }
        let (sequence, sources, session_id) = {
            let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            let Some(source) = shared.audio_sources.iter_mut().find(|s| s.id == source_id) else {
                return;
            };
            source.muted = muted;
            shared.saved_mutes.insert(source_id, muted);
            shared.sequence = shared.sequence.wrapping_add(1);
            (
                shared.sequence,
                shared.audio_sources.clone(),
                shared.session_id.clone(),
            )
        };
        self.sink.publish(EventEnvelope {
            sequence,
            session_id,
            event: EngineEvent::AudioSourcesChanged(sources),
        });
    }

    fn update_route(&self, source_id: AudioSourceId, route: AudioRoute) {
        if !matches!(self.recorder.state(), RecorderState::Idle) {
            return;
        }
        let (sequence, sources) = {
            let mut shared = self
                .shared
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let Some(source) = shared
                .audio_sources
                .iter_mut()
                .find(|source| source.id == source_id)
            else {
                return;
            };
            source.route = route;
            shared.saved_routes.insert(source_id, route);
            shared.sequence = shared.sequence.wrapping_add(1);
            (shared.sequence, shared.audio_sources.clone())
        };
        self.sink.publish(EventEnvelope {
            sequence,
            session_id: None,
            event: EngineEvent::AudioSourcesChanged(sources),
        });
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn configure_replay(&mut self, settings: ReplaySettings) {
        if !matches!(self.recorder.state(), RecorderState::Idle) {
            self.publish_replay_error(invalid_transition_error(
                "Stop capture before changing replay limits.".into(),
            ));
            return;
        }
        #[cfg(target_os = "windows")]
        if let Some(recording) = self.recording.as_mut()
            && let Err(error) = recording
                .configure_replay(replay_limit(settings).expect("validated replay settings"))
        {
            self.publish_replay_error(error);
            return;
        }
        self.replay_settings = settings;
        let (sequence, session_id) = {
            let mut shared = self
                .shared
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            shared.sequence = shared.sequence.wrapping_add(1);
            shared.replay_settings = settings;
            (shared.sequence, shared.session_id.clone())
        };
        self.sink.publish(EventEnvelope {
            sequence,
            session_id,
            event: EngineEvent::ReplayConfigured(settings),
        });
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn save_replay(&mut self, path: PathBuf, duration_seconds: u32) {
        if duration_seconds > self.replay_settings.duration_seconds {
            self.publish_replay_error(invalid_request_error(
                "Requested replay duration exceeds the configured window.".into(),
                "Choose a shorter duration.",
            ));
            return;
        }
        #[cfg(target_os = "windows")]
        let result = self
            .recording
            .as_ref()
            .and_then(NativeRecording::replay_buffer)
            .zip(self.replay_worker.as_ref())
            .ok_or_else(|| invalid_transition_error("Replay has no active native owner.".into()))
            .and_then(|(buffer, worker)| {
                worker.save_with_metadata(
                    buffer.clone(),
                    path,
                    duration_seconds,
                    if let Some(events) = self
                        .recording
                        .as_ref()
                        .and_then(NativeRecording::rolling_metadata)
                    {
                        let start = buffer.retained_start_ns().ok_or_else(|| {
                            RecorderError::invalid("Replay has no retained events range.")
                        })?;
                        let end = buffer.shared_end_ns().ok_or_else(|| {
                            RecorderError::invalid("Replay has no synchronized events range.")
                        })?;
                        Some(crate::replay_worker::ReplayMetadata::Rolling(
                            events.pin(start, end).map_err(crate::metadata::error)?,
                        ))
                    } else {
                        self.recording
                            .as_ref()
                            .and_then(NativeRecording::replay_metadata_path)
                            .map(crate::replay_worker::ReplayMetadata::File)
                    },
                )
            });
        #[cfg(not(target_os = "windows"))]
        let result: Result<(), RecorderError> = {
            let _ = path;
            Err(RecorderError::invalid(
                "Replay is unavailable on this platform.",
            ))
        };
        if let Err(error) = result {
            self.publish_replay_error(error);
        }
    }

    #[cfg(target_os = "windows")]
    fn publish_replay_results(&self, results: Vec<Result<ReplaySaveResult, RecorderError>>) {
        for result in results {
            match result {
                Ok(result) => {
                    let (sequence, session_id) = {
                        let mut shared = self.shared.lock().unwrap_or_else(|p| p.into_inner());
                        shared.sequence = shared.sequence.wrapping_add(1);
                        shared.last_replay = Some(result.clone());
                        (shared.sequence, shared.session_id.clone())
                    };
                    self.sink.publish(EventEnvelope {
                        sequence,
                        session_id,
                        event: EngineEvent::ReplaySaved(result),
                    });
                }
                Err(error) => self.publish_replay_error(error),
            }
        }
    }

    fn close_replay_worker(&mut self) {
        #[cfg(target_os = "windows")]
        if let Some(mut worker) = self.replay_worker.take() {
            self.publish_replay_results(worker.shutdown());
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn publish_replay_error(&self, error: RecorderError) {
        let (sequence, session_id) = {
            let mut shared = self
                .shared
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            shared.sequence = shared.sequence.wrapping_add(1);
            (shared.sequence, shared.session_id.clone())
        };
        self.sink.publish(EventEnvelope {
            sequence,
            session_id,
            event: EngineEvent::ReplaySaveFailed(error),
        });
    }

    fn publish_command_error(&self, error: RecorderError) {
        let (sequence, session_id) = {
            let mut shared = self
                .shared
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            shared.sequence = shared.sequence.wrapping_add(1);
            (shared.sequence, shared.session_id.clone())
        };
        self.sink.publish(EventEnvelope {
            sequence,
            session_id,
            event: EngineEvent::Warning(EngineWarning {
                code: "shortcutCommandFailed".into(),
                message: error.message,
                action: error.action,
            }),
        });
    }

    fn store_plan(&self, decisions: Vec<String>) -> EventEnvelope {
        let (sequence, session_id) = {
            let mut shared = self
                .shared
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            shared.sequence = shared.sequence.wrapping_add(1);
            shared.plan_decisions = decisions.clone();
            (shared.sequence, shared.session_id.clone())
        };
        EventEnvelope {
            sequence,
            session_id,
            event: EngineEvent::PlanDecisions(decisions),
        }
    }

    /// Closes admission and asks the native owner to drain. Idempotent by lifecycle.
    fn stop(&mut self) {
        if !matches!(
            self.recorder.state(),
            RecorderState::Preparing | RecorderState::Recording | RecorderState::Paused
        ) {
            return;
        }
        if let Err(error) = self.recorder.stop() {
            self.fail_session(error);
            return;
        }
        if self.countdown.take().is_some() {
            let session = self.session.as_ref().expect("countdown has a session").id;
            if let Err(error) = self.recorder.finalized(session) {
                self.fail_session(error);
                return;
            }
            self.session = None;
            #[cfg(target_os = "windows")]
            self.restore_idle_preview();
            self.publish_state(RecorderState::Idle);
            return;
        }
        if let (Some(session), Some(recording)) = (self.session.as_mut(), self.recording.as_ref()) {
            session.freeze_elapsed(recording);
        }
        self.publish_metrics(true);
        self.publish_state(RecorderState::Finalizing);
        let outcome = match self.recording.as_mut() {
            Some(recording) => recording.request_stop(),
            None => Err(internal_error(
                "The lifecycle was finalizing without a native recording to drain.".to_string(),
            )),
        };
        if let Err(error) = outcome {
            self.fail_recording(error);
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn pause(&mut self) {
        if self.recorder.state() != &RecorderState::Recording {
            return;
        }
        match self
            .recording
            .as_mut()
            .expect("recording state owns native session")
            .pause()
        {
            Ok(()) => {
                let _ = self.recorder.pause();
                self.publish_metrics(true);
                self.publish_state(RecorderState::Paused);
            }
            Err(error) => self.fail_recording(error),
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn resume(&mut self) {
        if self.recorder.state() != &RecorderState::Paused {
            return;
        }
        match self
            .recording
            .as_mut()
            .expect("paused state owns native session")
            .resume()
        {
            Ok(()) => {
                let _ = self.recorder.resume();
                self.publish_state(RecorderState::Recording);
                self.publish_metrics(true);
            }
            Err(error) => self.fail_recording(error),
        }
    }

    /// Returns a failed recording to idle, after its native resources were released.
    fn reset(&mut self) {
        let state = self.recorder.state().clone();
        match state {
            RecorderState::Failed(_) => {
                // Already released by the failure path; this is idempotent.
                self.release_recording();
                if let Err(error) = self.recorder.reset_after_cleanup() {
                    self.publish_state(RecorderState::Failed(error));
                    return;
                }
                self.session = None;
                #[cfg(target_os = "windows")]
                self.restore_idle_preview();
                self.publish_state(RecorderState::Idle);
            }
            // A request that was refused before a session began leaves this owner idle
            // while the interface reported it as failed; the reset only republishes idle.
            RecorderState::Idle => {
                self.session = None;
                #[cfg(target_os = "windows")]
                self.restore_idle_preview();
                self.publish_state(RecorderState::Idle);
            }
            _ => {}
        }
    }

    /// One bounded step of the native owner, driven by the authoritative lifecycle.
    fn advance(&mut self) {
        let state = self.recorder.state().clone();
        let outcome = match state {
            RecorderState::Recording | RecorderState::Paused | RecorderState::Finalizing => {
                match self.recording.as_mut() {
                    Some(recording) => poll_native(recording),
                    None => Err(internal_error(
                        "The lifecycle owns a session without a native recording to advance."
                            .to_string(),
                    )),
                }
            }
            _ => return,
        };
        match outcome {
            Ok(NativeStep::Recording) => {}
            Ok(NativeStep::Drained) => {
                if self.recorder.state() == &RecorderState::Finalizing {
                    self.finish();
                } else {
                    self.fail_recording(internal_error(
                        "The encoder reported a completed drain before this recording was asked to stop."
                            .to_string(),
                    ));
                }
            }
            Err(error) => self.fail_recording(error),
        }
    }

    /// Completes a drained recording and publishes the one artifact.
    ///
    /// The artifact event is published while the finished session is still the published
    /// session, and the idle state that follows ends it. Nothing publishes an artifact
    /// before this point: a failure, an abort or a shutdown releases the owner without
    /// finalizing the container.
    fn finish(&mut self) {
        // The session's last telemetry is measured here, while it is still finalizing and
        // still owns its clock, so the elapsed time the interface keeps is this session's
        // whole active media time instead of the last 4 Hz sample.
        self.publish_metrics(true);
        let Some(mut recording) = self.recording.take() else {
            self.fail_session(internal_error(
                "A drained recording had no native owner to finalize.".to_string(),
            ));
            return;
        };
        #[cfg(target_os = "windows")]
        if let (Some(buffer), Some(worker)) =
            (recording.replay_buffer(), self.replay_worker.as_mut())
        {
            worker.checkpoint_final(buffer);
        }
        if self.replay_only {
            match recording.abort() {
                Ok(()) => {
                    self.close_replay_worker();
                    if let Some(session) = self.session.as_ref() {
                        let _ = self.recorder.finalized(session.id);
                    }
                    self.session = None;
                    #[cfg(target_os = "windows")]
                    {
                        self.clear_preview();
                        self.meter.end();
                        self.restore_idle_preview();
                    }
                    self.publish_state(RecorderState::Idle);
                }
                Err(error) => {
                    drop(recording);
                    self.close_replay_worker();
                    self.fail_session(error);
                }
            }
            return;
        }
        match recording.finish() {
            Ok(artifact) => {
                #[cfg(target_os = "windows")]
                self.clear_preview();
                let result = self
                    .session
                    .as_ref()
                    .ok_or_else(|| internal_error("Finalized recording has no session.".into()))
                    .and_then(|session| self.recorder.finalized(session.id));
                if let Err(error) = result {
                    self.fail_session(error);
                    return;
                }
                self.close_replay_worker();
                self.publish_artifact(artifact);
                #[cfg(target_os = "windows")]
                self.meter.end();
                self.session = None;
                #[cfg(target_os = "windows")]
                self.restore_idle_preview();
                self.publish_state(RecorderState::Idle);
            }
            // `finish` already released every resource on this path.
            Err(error) => {
                drop(recording);
                #[cfg(target_os = "windows")]
                self.clear_preview();
                self.fail_session(error);
            }
        }
    }

    /// Releases the native owner without finalizing anything. Idempotent.
    fn release_recording(&mut self) {
        if let Some(mut recording) = self.recording.take() {
            let _ = recording.abort();
        }
        self.close_replay_worker();
        #[cfg(target_os = "windows")]
        self.clear_preview();
        #[cfg(target_os = "windows")]
        self.meter.end();
    }

    #[cfg(target_os = "windows")]
    fn clear_preview(&self) {
        let mut publication = self
            .preview
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(hub) = publication.hub.take() {
            hub.close();
        }
        publication.error = None;
    }

    /// Reports a failure that owns a session: the native owner is released first, so a
    /// failure never leaves a capture session or an open container behind.
    fn fail_recording(&mut self, mut error: RecorderError) {
        if let Some(mut recording) = self.recording.take()
            && let Err(cleanup) = recording.abort()
        {
            error
                .message
                .push_str(&format!(" Cleanup: {}", cleanup.message));
        }
        #[cfg(target_os = "windows")]
        self.clear_preview();
        self.fail_session(error);
    }

    /// Records a sticky failure in the authoritative lifecycle and publishes it.
    fn fail_session(&mut self, error: RecorderError) {
        self.close_replay_worker();
        #[cfg(target_os = "windows")]
        self.meter.end();
        if let Some(session) = self.session.as_ref().map(|session| session.id) {
            // A failing lifecycle keeps its first failure; a second report is not allowed
            // to replace it.
            let _ = self.recorder.fail(session, error.clone());
        }
        self.publish_state(RecorderState::Failed(error));
    }

    /// Reports a request that could not begin a session.
    ///
    /// The published state is the interface-visible truth, so a request that was refused
    /// after it was accepted is reported as a failure with its own error rather than being
    /// dropped silently. [`RecordingControl::start`] refuses the same request before it is
    /// queued, so this path is a safety net.
    fn reject(&mut self, error: RecorderError) {
        self.publish_state(RecorderState::Failed(error));
    }

    /// Publishes one state as both the snapshot authority and one event.
    ///
    /// The state is stored first and the event second, under one lock for the sequence, so
    /// a snapshot taken at any instant and the event stream can never disagree about the
    /// same sequence. A published `preparing` state starts one new session, so it is also
    /// where the previous session's telemetry and finalized artifact are cleared: nothing
    /// that belongs to a finished recording can be read as this session's result.
    fn publish_state(&self, state: RecorderState) {
        self.sink.publish(self.store_state(state));
    }

    /// Stores publication while readiness holds admission; callbacks run after unlocking.
    fn store_state(&self, state: RecorderState) -> EventEnvelope {
        #[cfg(target_os = "windows")]
        crate::encoder_calibration::recording_state(&state);
        let session_id = self.session.as_ref().map(ActiveSession::label);
        let begins_session = matches!(state, RecorderState::Preparing);
        let (sequence, session_id) = {
            let mut shared = self
                .shared
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            shared.sequence = shared.sequence.wrapping_add(1);
            shared.session_id = session_id;
            if begins_session {
                shared.metrics = None;
                shared.last_artifact = None;
                shared.last_replay = None;
                shared.plan_decisions.clear();
            }
            shared.replay_only = self.replay_only
                && !matches!(state, RecorderState::Idle | RecorderState::Failed(_));
            #[cfg(target_os = "windows")]
            {
                shared.replay_active = self.replay_worker.is_some()
                    && matches!(state, RecorderState::Recording | RecorderState::Paused);
            }
            shared.state = state.clone();
            if begins_session || matches!(state, RecorderState::Failed(_)) {
                // Hand the reservation to published state while holding its lock.
                self.start_pending.store(false, Ordering::Release);
            }
            (shared.sequence, shared.session_id.clone())
        };
        EventEnvelope {
            sequence,
            session_id,
            event: EngineEvent::RecorderStateChanged(state),
        }
    }

    /// Publishes one measured telemetry event, at most once per [`METRICS_INTERVAL`].
    ///
    /// `force` publishes at lifecycle boundaries as well as at session start. A session
    /// without a native clock publishes nothing.
    fn publish_metrics(&mut self, force: bool) {
        if !force && self.recorder.state() != &RecorderState::Recording {
            return;
        }
        if !matches!(
            self.recorder.state(),
            RecorderState::Recording | RecorderState::Paused | RecorderState::Finalizing
        ) {
            return;
        }
        let now = Instant::now();
        if !force && !metrics_due(self.last_metrics, now) {
            return;
        }
        let metrics = match (self.session.as_mut(), self.recording.as_ref()) {
            (Some(session), Some(recording)) => session.metrics(recording),
            // The lifecycle and the native owner disagree, so there is nothing to measure.
            _ => None,
        };
        let Some(metrics) = metrics else {
            return;
        };
        self.last_metrics = Some(now);
        self.publish_metrics_event(metrics);
        if self.recorder.state() == &RecorderState::Recording {
            self.publish_audio_source_lifecycle();
        }
        #[cfg(target_os = "windows")]
        if let (Some(session), Some(NativeRecording::Audio(recording))) =
            (self.session.as_mut(), self.recording.as_ref())
            && !session.audio_clock_warning_sent
            && let Some(health) = recording.audio_clock_health().into_iter().find(|health| {
                health.drift.reliable
                    && health.drift.rate_ppm.unsigned_abs() >= 100
                    && health.drift.residual_ns.unsigned_abs() >= 20_000_000
            })
        {
            session.audio_clock_warning_sent = true;
            let compensation = recording
                .audio_source_lifecycle()
                .into_iter()
                .find(|source| source.source_id == health.source_id)
                .and_then(|source| source.compensation);
            let label = session.label();
            let sequence = {
                let mut shared = self.shared.lock().unwrap_or_else(|p| p.into_inner());
                shared.sequence = shared.sequence.wrapping_add(1);
                shared.sequence
            };
            self.sink.publish(EventEnvelope {
                sequence,
                session_id: Some(label),
                event: EngineEvent::Warning(EngineWarning {
                    code: "audioClockDrift".into(),
                    message: audio_clock_warning(&health, compensation),
                    action: "Check synchronization when editing this recording; source clock error is separate from capture frame pacing.".into(),
                }),
            });
        }
    }

    fn publish_audio_source_lifecycle(&mut self) {
        use frame_audio::SourceLifecycle;
        #[cfg(target_os = "windows")]
        let sources = match self.recording.as_ref() {
            Some(NativeRecording::Audio(recording)) => recording.audio_source_lifecycle(),
            _ => Vec::new(),
        };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let sources = self
            .recording
            .as_ref()
            .map_or_else(Vec::new, |recording| recording.audio_source_lifecycle());
        #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
        let sources: Vec<frame_audio::SourceLifecycleSnapshot> = Vec::new();
        let Some(session) = self.session.as_mut() else {
            return;
        };
        for source in sources {
            let previous = session
                .audio_source_states
                .insert(source.source_id.clone(), source.state);
            let changed = match (previous, source.state) {
                (None, SourceLifecycle::Connected { .. }) => false,
                (
                    Some(SourceLifecycle::Reconnecting { .. }),
                    SourceLifecycle::Reconnecting { .. },
                ) => false,
                (previous, state) => previous != Some(state),
            };
            if !changed {
                continue;
            }
            let (code, message) = match source.state {
                SourceLifecycle::Connected { .. } => {
                    ("audioSourceRestored", "Audio capture resumed")
                }
                SourceLifecycle::Reconnecting { .. } => {
                    ("audioSourceReconnecting", "Audio capture is reconnecting")
                }
                SourceLifecycle::Ended { .. } => ("audioSourceEnded", "Audio capture ended"),
            };
            let sequence = {
                let mut shared = self.shared.lock().unwrap_or_else(|p| p.into_inner());
                shared.sequence = shared.sequence.wrapping_add(1);
                shared.sequence
            };
            self.sink.publish(EventEnvelope {
                sequence,
                session_id: Some(session.label()),
                event: EngineEvent::Warning(EngineWarning {
                    code: code.into(),
                    message: format!("{message}: {}. Other recording sources continue; track identities are preserved.", source.source_id.0),
                    action: "Check this source's audio and device connection. Missing intervals remain silence in its existing track.".into(),
                }),
            });
        }
    }

    /// Stores one telemetry value and then publishes it, in that order.
    fn publish_metrics_event(&self, metrics: RecorderMetrics) {
        let (sequence, session_id) = {
            let mut shared = self
                .shared
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            shared.sequence = shared.sequence.wrapping_add(1);
            shared.metrics = Some(metrics.clone());
            (shared.sequence, shared.session_id.clone())
        };
        self.sink.publish(EventEnvelope {
            sequence,
            session_id,
            event: EngineEvent::Metrics(Box::new(metrics)),
        });
    }

    fn poll_recording_storage(&mut self) {
        if !matches!(
            self.recorder.state(),
            RecorderState::Preparing | RecorderState::Recording | RecorderState::Paused
        ) {
            return;
        }
        let (Some(session), Some(directory), Some(monitor)) = (
            &self.session,
            &self.storage_directory,
            &mut self.storage_monitor,
        ) else {
            return;
        };
        let now = Instant::now();
        if !monitor.due(now) {
            return;
        }
        let label = session.label();
        let Some(warning) = monitor.poll(&label, directory, now) else {
            return;
        };
        let sequence = {
            let mut shared = self.shared.lock().unwrap_or_else(|p| p.into_inner());
            shared.sequence = shared.sequence.wrapping_add(1);
            shared.sequence
        };
        self.sink.publish(EventEnvelope {
            sequence,
            session_id: Some(label),
            event: EngineEvent::Warning(warning),
        });
    }

    /// Publishes the canonical finalized artifact, before the idle state that ends the
    /// session.
    ///
    /// The artifact is stored as the published snapshot state before the event is emitted,
    /// so a refresh right after a success still reports the file. It is the only place
    /// that ever fills it.
    fn publish_artifact(&self, artifact: RecordingArtifact) {
        let (sequence, session_id) = {
            let mut shared = self
                .shared
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            shared.sequence = shared.sequence.wrapping_add(1);
            shared.last_artifact = Some(artifact.clone());
            (shared.sequence, shared.session_id.clone())
        };
        self.sink.publish(EventEnvelope {
            sequence,
            session_id,
            event: EngineEvent::RecordingFinalized(artifact),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frame_contracts::{DisplayId, OutputSettings};
    use frame_contracts::{FpsMode, Resolution};
    use std::sync::Mutex as StdMutex;

    #[test]
    fn drift_warning_reports_actual_compensation_state_and_not_content_offset() {
        let health = frame_audio::SourceClockHealth {
            source_id: frame_contracts::AudioSourceId("fixture".into()),
            drift: frame_clock::DriftEstimate {
                rate_ppm: 200,
                residual_ns: 24_000_000,
                window_ns: 10_000_000_000,
                stable_windows: 3,
                reliable: true,
            },
        };
        let disabled = audio_clock_warning(&health, None);
        assert!(disabled.contains("correction is disabled"));
        assert!(!disabled.contains("unavailable"));
        assert!(disabled.contains("not a measured content A/V offset"));
        for reliable in [false, true] {
            let enabled = audio_clock_warning(
                &health,
                Some(frame_audio::DriftCompensationHealth {
                    enabled: true,
                    applied_milli_ppm: -125_500,
                    reliable,
                }),
            );
            assert!(enabled.contains("correction is enabled"));
            assert!(enabled.contains("applied -125.500 ppm"));
            assert_eq!(enabled.contains("not yet reliable"), !reliable);
            assert!(!enabled.contains("unavailable"));
        }
    }

    #[cfg(target_os = "windows")]
    mod fix002 {
        use super::*;
        use std::sync::atomic::AtomicUsize;
        use std::sync::mpsc::{Sender, channel};

        const WAIT: Duration = Duration::from_secs(5);

        #[derive(Default)]
        pub(super) struct Witness {
            acquired: AtomicUsize,
            stops: AtomicUsize,
            drops: AtomicUsize,
            joins: AtomicUsize,
            polls: AtomicUsize,
            stop_requests: AtomicUsize,
            later_stages: AtomicUsize,
        }

        pub(in crate::recording_control) struct Owner {
            witness: Arc<Witness>,
            closed: bool,
            cleanup_failure: bool,
            clean_early_stop: bool,
            metadata_start_error: Option<RecorderError>,
            ready: Option<(Sender<()>, Receiver<()>)>,
            native_job: Option<JoinHandle<()>>,
            joining: Option<Sender<()>>,
            polled: Option<Sender<()>>,
        }

        impl Owner {
            fn new(witness: Arc<Witness>) -> Self {
                witness.acquired.fetch_add(1, Ordering::SeqCst);
                Self {
                    witness,
                    closed: false,
                    cleanup_failure: false,
                    clean_early_stop: false,
                    metadata_start_error: None,
                    ready: None,
                    native_job: None,
                    joining: None,
                    polled: None,
                }
            }

            pub(in crate::recording_control) fn abort(&mut self) -> Result<(), RecorderError> {
                if !self.closed {
                    self.closed = true;
                    self.witness.stops.fetch_add(1, Ordering::SeqCst);
                    if let Some(joining) = self.joining.take() {
                        joining.send(()).unwrap();
                    }
                    if let Some(job) = self.native_job.take() {
                        job.join().unwrap();
                    }
                    self.witness.joins.fetch_add(1, Ordering::SeqCst);
                    if self.cleanup_failure {
                        return Err(internal_error("scripted cleanup failed".into()));
                    }
                    if self.clean_early_stop {
                        return Err(RecorderError {
                            code: ErrorCode::FinalizationFailed,
                            message: "The encoder stopped without completing the stream.".into(),
                            recovery: RecoveryStatus::NoRecording,
                            action: "Discard the incomplete stream.".into(),
                        });
                    }
                }
                Ok(())
            }

            pub(in crate::recording_control) fn configure_replay(
                &mut self,
            ) -> Result<(), RecorderError> {
                if let Some((entered, release)) = self.ready.take() {
                    entered.send(()).unwrap();
                    release.recv_timeout(WAIT).expect("release readiness");
                }
                Ok(())
            }

            pub(in crate::recording_control) fn enable_metadata(
                &self,
            ) -> Result<(), RecorderError> {
                self.metadata_start_error.clone().map_or(Ok(()), Err)
            }

            pub(in crate::recording_control) fn request_stop(
                &mut self,
            ) -> Result<(), RecorderError> {
                self.witness.stop_requests.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }

            pub(in crate::recording_control) fn poll(&mut self) -> NativeStep {
                self.witness.polls.fetch_add(1, Ordering::SeqCst);
                if let Some(sender) = self.polled.take() {
                    sender.send(()).unwrap();
                }
                NativeStep::Recording
            }
        }

        impl Drop for Owner {
            fn drop(&mut self) {
                let _ = self.abort();
                self.witness.drops.fetch_add(1, Ordering::SeqCst);
            }
        }

        struct Sink {
            observed: RecordingSink,
            states: Sender<RecorderState>,
            preparing: StdMutex<Option<(Sender<()>, Receiver<()>)>>,
        }

        impl EngineEventSink for Sink {
            fn publish(&self, envelope: EventEnvelope) {
                self.observed.publish(envelope.clone());
                if let EngineEvent::RecorderStateChanged(state) = envelope.event {
                    self.states.send(state.clone()).unwrap();
                    if state == RecorderState::Preparing
                        && let Some((entered, release)) = self.preparing.lock().unwrap().take()
                    {
                        entered.send(()).unwrap();
                        release
                            .recv_timeout(WAIT)
                            .expect("release Preparing publication");
                    }
                }
            }
        }

        fn sink() -> (Arc<Sink>, Receiver<RecorderState>) {
            let (send, recv) = channel();
            (
                Arc::new(Sink {
                    observed: RecordingSink::default(),
                    states: send,
                    preparing: StdMutex::new(None),
                }),
                recv,
            )
        }

        fn state(states: &Receiver<RecorderState>, wanted: RecorderState) {
            let got = states
                .recv_timeout(WAIT)
                .expect("bounded state publication");
            assert_eq!(got, wanted);
        }

        fn balanced(witness: &Witness, count: usize) {
            assert_eq!(witness.acquired.load(Ordering::SeqCst), count);
            assert_eq!(witness.stops.load(Ordering::SeqCst), count);
            assert_eq!(witness.joins.load(Ordering::SeqCst), count);
            assert_eq!(witness.drops.load(Ordering::SeqCst), count);
        }

        fn no_recording(sink: &Sink, witness: &Witness) {
            assert!(!sink.observed.states().contains(&RecorderState::Recording));
            assert!(sink.observed.artifacts().is_empty());
            assert!(
                !sink
                    .observed
                    .envelopes()
                    .iter()
                    .any(|event| matches!(event.event, EngineEvent::PlanDecisions(_)))
            );
            assert_eq!(witness.polls.load(Ordering::SeqCst), 0);
            assert_eq!(witness.stop_requests.load(Ordering::SeqCst), 0);
        }

        fn started(owner: Owner) -> NativeStart {
            let candidates = [EncoderCandidate {
                id: "device-free".into(),
                codec: "h264".into(),
                hardware: true,
                limits: EncoderLimits {
                    max_width: 4096,
                    max_height: 4096,
                    cbr: true,
                    vbr: true,
                },
            }];
            let plan = SessionPlan::freeze(
                frame_settings::plan_recording(&PlanInputs {
                    quality: frame_contracts::Quality::High,
                    resolution: Resolution::Native,
                    fps_mode: FpsMode::AutoMatchSource,
                    encoder: EncoderPreference::Auto,
                    native: Dimensions {
                        width: 64,
                        height: 48,
                    },
                    nominal_rate: NominalRate::new(
                        NonZeroU32::new(30).unwrap(),
                        NonZeroU32::new(1).unwrap(),
                    ),
                    candidates: &candidates,
                })
                .unwrap(),
            );
            let clock = Arc::new(MasterClock::default());
            let origin = clock.now();
            (
                NativeRecording::Scripted(Box::new(owner)),
                SessionTiming {
                    clock,
                    origin,
                    requested_fps: Some(30.0),
                    source_rate_hz: Some(30.0),
                },
                plan,
                vec![],
                None,
            )
        }

        fn fixture_request(name: &str) -> RecordingRequest {
            let directory = std::env::temp_dir().join("freym-control-preparation-fix-002");
            std::fs::create_dir_all(&directory).unwrap();
            request(
                &directory.to_string_lossy(),
                &format!(
                    "{name}-{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                ),
            )
        }

        #[test]
        fn metadata_warning_reentry_runs_after_admission_unlock_and_preserves_event_order() {
            struct ReentrantSink {
                observed: RecordingSink,
                control: StdMutex<std::sync::Weak<RecordingControl>>,
                reported: Sender<(bool, Result<(), RecorderError>)>,
            }
            impl EngineEventSink for ReentrantSink {
                fn publish(&self, envelope: EventEnvelope) {
                    self.observed.publish(envelope.clone());
                    if matches!(&envelope.event, EngineEvent::Warning(warning)
                        if warning.code == "interactionMetadataUnavailable")
                    {
                        let control = self.control.lock().unwrap().upgrade().unwrap();
                        let unlocked = control.command_gate.try_lock().is_ok();
                        let result = if unlocked {
                            control.stop()
                        } else {
                            Err(internal_error("callback held admission".into()))
                        };
                        self.reported.send((unlocked, result)).unwrap();
                    }
                }
            }
            let (reported, result) = channel();
            let sink = Arc::new(ReentrantSink {
                observed: RecordingSink::default(),
                control: StdMutex::new(std::sync::Weak::new()),
                reported,
            });
            let witness = Arc::new(Witness::default());
            let seen = witness.clone();
            let control = Arc::new(RecordingControl::new_inner(
                sink.clone(),
                Some(Box::new(move |_| {
                    let mut owner = Owner::new(seen.clone());
                    owner.metadata_start_error =
                        Some(internal_error("metadata unavailable".into()));
                    Ok(started(owner))
                })),
            ));
            *sink.control.lock().unwrap() = Arc::downgrade(&control);
            control.configure_interaction_metadata(true).unwrap();
            control.start(fixture_request("metadata-reentry")).unwrap();
            let (unlocked, stopped) = result.recv_timeout(WAIT).unwrap();
            control.shutdown();
            assert!(unlocked);
            stopped.unwrap();
            balanced(&witness, 1);
            let events = sink.observed.envelopes();
            assert!(
                events
                    .windows(2)
                    .all(|pair| pair[0].sequence < pair[1].sequence)
            );
            let warning = events
                .iter()
                .position(|event| {
                    matches!(
                        &event.event, EngineEvent::Warning(value)
                        if value.code == "interactionMetadataUnavailable"
                    )
                })
                .unwrap();
            assert!(matches!(
                events[warning + 1].event,
                EngineEvent::PlanDecisions(_)
            ));
            assert_eq!(
                events[warning + 2].event,
                EngineEvent::RecorderStateChanged(RecorderState::Recording)
            );
        }

        #[test]
        fn fix002_stop_before_preparation_and_fresh_start_resets_cancellation() {
            let (sink, states) = sink();
            let witness = Arc::new(Witness::default());
            let (entered, blocked) = channel();
            let (release, proceed) = channel();
            *sink.preparing.lock().unwrap() = Some((entered, proceed));
            let seen = witness.clone();
            let (polled, poll_event) = channel();
            let mut polled = Some(polled);
            let control = RecordingControl::new_inner(
                sink.clone(),
                Some(Box::new(move |cancel| {
                    check_preparation_cancelled(cancel)?;
                    let mut owner = Owner::new(seen.clone());
                    owner.polled = polled.take();
                    Ok(started(owner))
                })),
            );
            control.start(fixture_request("fix002-before")).unwrap();
            blocked.recv_timeout(WAIT).unwrap();
            control.stop().unwrap();
            assert!(control.cancel_start.load(Ordering::Acquire));
            release.send(()).unwrap();
            state(&states, RecorderState::Preparing);
            state(&states, RecorderState::Idle);
            no_recording(&sink, &witness);
            balanced(&witness, 0);
            control.start(fixture_request("fix002-fresh")).unwrap();
            state(&states, RecorderState::Preparing);
            state(&states, RecorderState::Recording);
            poll_event.recv_timeout(WAIT).unwrap();
            assert!(!control.cancel_start.load(Ordering::Acquire));
            control.shutdown();
            balanced(&witness, 1);
        }

        #[test]
        fn fix002_stop_during_blocked_step_joins_before_idle() {
            let (sink, states) = sink();
            let witness = Arc::new(Witness::default());
            let seen = witness.clone();
            let (entered, blocked) = channel();
            let (release, step) = channel();
            let (join_release, native_step) = channel();
            let (joining, join_entered) = channel();
            let mut native_step = Some(native_step);
            let mut joining = Some(joining);
            let control = RecordingControl::new_inner(
                sink.clone(),
                Some(Box::new(move |cancel| {
                    let mut owner = Owner::new(seen.clone());
                    let native_step = native_step.take().unwrap();
                    owner.native_job = Some(std::thread::spawn(move || {
                        native_step
                            .recv_timeout(WAIT)
                            .expect("bounded fake native call");
                    }));
                    owner.joining = joining.take();
                    entered.send(()).unwrap();
                    step.recv_timeout(WAIT).unwrap();
                    if let Err(error) = check_preparation_cancelled(cancel) {
                        owner.abort()?;
                        drop(owner);
                        return Err(error);
                    }
                    seen.later_stages.fetch_add(1, Ordering::SeqCst);
                    Ok(started(owner))
                })),
            );
            control.start(fixture_request("fix002-blocked")).unwrap();
            blocked.recv_timeout(WAIT).unwrap();
            control.stop().unwrap();
            control.stop().unwrap(); // duplicate admission remains idempotent
            assert!(control.cancel_start.load(Ordering::Acquire));
            release.send(()).unwrap();
            join_entered.recv_timeout(WAIT).unwrap();
            assert_eq!(control.published().state, RecorderState::Preparing);
            assert_eq!(witness.joins.load(Ordering::SeqCst), 0);
            join_release.send(()).unwrap();
            state(&states, RecorderState::Preparing);
            state(&states, RecorderState::Idle);
            control.shutdown();
            balanced(&witness, 1);
            no_recording(&sink, &witness);
            assert_eq!(witness.later_stages.load(Ordering::SeqCst), 0);
        }

        fn before_ready(partial: bool, cleanup_failure: bool, clean_early_stop: bool, name: &str) {
            let (sink, states) = sink();
            let witness = Arc::new(Witness::default());
            let seen = witness.clone();
            let (entered, blocked) = channel();
            let (release, step) = channel();
            let mut ready = Some((entered, step));
            let request = fixture_request(name);
            let output = output_path(&request).unwrap();
            let partial_path = output.clone();
            let control = RecordingControl::new_inner(
                sink.clone(),
                Some(Box::new(move |_| {
                    let mut owner = Owner::new(seen.clone());
                    if partial {
                        std::fs::write(&partial_path, b"incomplete-prefix").unwrap();
                    }
                    owner.ready = ready.take();
                    owner.cleanup_failure = cleanup_failure;
                    owner.clean_early_stop = clean_early_stop;
                    Ok(started(owner))
                })),
            );
            control.start(request).unwrap();
            blocked.recv_timeout(WAIT).unwrap(); // owner returned, replay config just before ready
            control.stop().unwrap();
            assert!(control.cancel_start.load(Ordering::Acquire));
            release.send(()).unwrap();
            state(&states, RecorderState::Preparing);
            if cleanup_failure {
                let terminal = states.recv_timeout(WAIT).unwrap();
                let RecorderState::Failed(error) = terminal else {
                    panic!("cleanup failure must stay visible");
                };
                assert!(error.message.contains("scripted cleanup failed"));
                assert_eq!(error.recovery, RecoveryStatus::Unknown);
            } else {
                state(&states, RecorderState::Idle);
            }
            control.shutdown();
            balanced(&witness, 1);
            no_recording(&sink, &witness);
            if partial {
                assert_eq!(std::fs::read(&output).unwrap(), b"incomplete-prefix");
                assert_eq!(sink.observed.envelopes().iter().filter(|e| matches!(
                    &e.event, EngineEvent::Warning(w) if w.code == "preparationCancelledPartial"
                )).count(), usize::from(!cleanup_failure));
            } else {
                assert!(!output.exists());
            }
        }

        #[test]
        fn fix002_stop_immediately_before_ready() {
            before_ready(false, false, false, "fix002-ready");
        }

        #[test]
        fn fix002_created_output_is_partial_without_artifact() {
            before_ready(true, false, false, "fix002-partial");
        }

        #[test]
        fn fix002_cleanup_failure_is_not_reported_as_idle() {
            before_ready(true, true, false, "fix002-cleanup");
        }

        #[test]
        fn fix002_clean_undrained_encoder_abort_still_returns_idle() {
            before_ready(false, false, true, "fix002-clean-undrained");
        }

        #[test]
        fn fix002_success_and_ready_winner_keep_ordinary_stop() {
            let (sink, states) = sink();
            let witness = Arc::new(Witness::default());
            let seen = witness.clone();
            let (polled, poll_event) = channel();
            let mut polled = Some(polled);
            let control = RecordingControl::new_inner(
                sink.clone(),
                Some(Box::new(move |_| {
                    let mut owner = Owner::new(seen.clone());
                    owner.polled = polled.take();
                    Ok(started(owner))
                })),
            );
            control.start(fixture_request("fix002-success")).unwrap();
            state(&states, RecorderState::Preparing);
            state(&states, RecorderState::Recording);
            poll_event.recv_timeout(WAIT).unwrap();
            control.stop().unwrap();
            state(&states, RecorderState::Finalizing);
            assert!(!control.cancel_start.load(Ordering::Acquire));
            control.shutdown();
            balanced(&witness, 1);
            assert_eq!(witness.stop_requests.load(Ordering::SeqCst), 1);
            assert!(witness.polls.load(Ordering::SeqCst) >= 1);
            assert_eq!(
                sink.observed
                    .states()
                    .iter()
                    .filter(|s| **s == RecorderState::Recording)
                    .count(),
                1
            );
            assert!(sink.observed.artifacts().is_empty());
        }

        #[test]
        fn fix002_reserved_idle_stop_marks_cancellation_before_queueing() {
            let (send, recv) = sync_channel(COMMAND_CAPACITY);
            let control = RecordingControl {
                commands: send,
                command_wake: None,
                command_gate: Arc::new(Mutex::new(())),
                shutdown: Arc::new(AtomicBool::new(false)),
                shared: Arc::new(Mutex::new(PublishedState::default())),
                worker: Mutex::new(None),
                start_pending: Arc::new(AtomicBool::new(true)),
                stop_pending: AtomicBool::new(false),
                cancel_start: Arc::new(AtomicBool::new(false)),
                preview: Arc::new(Mutex::new(PreviewPublication::default())),
                meter: Arc::new(MeterHub::default()),
            };
            control.stop().unwrap();
            assert!(control.cancel_start.load(Ordering::Acquire));
            assert!(matches!(
                recv.recv_timeout(WAIT).unwrap(),
                ControlCommand::Stop
            ));
        }
    }

    #[cfg(target_os = "windows")]
    pub(super) use fix002::Owner as Fix002Owner;

    /// The observer an example or a test uses instead of a window.
    #[derive(Default)]
    struct RecordingSink {
        envelopes: StdMutex<Vec<EventEnvelope>>,
    }

    impl RecordingSink {
        fn envelopes(&self) -> Vec<EventEnvelope> {
            self.envelopes
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone()
        }

        fn states(&self) -> Vec<RecorderState> {
            self.envelopes()
                .into_iter()
                .filter_map(|envelope| match envelope.event {
                    EngineEvent::RecorderStateChanged(state) => Some(state),
                    _ => None,
                })
                .collect()
        }

        fn artifacts(&self) -> Vec<RecordingArtifact> {
            self.envelopes()
                .into_iter()
                .filter_map(|envelope| match envelope.event {
                    EngineEvent::RecordingFinalized(artifact) => Some(artifact),
                    _ => None,
                })
                .collect()
        }
    }

    impl EngineEventSink for RecordingSink {
        fn publish(&self, envelope: EventEnvelope) {
            self.envelopes
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push(envelope);
        }
    }

    /// Calls publishers on the real worker without running its loop or opening devices.
    #[cfg(target_os = "windows")]
    fn fix007_worker(
        sink: Arc<RecordingSink>,
        shared: Arc<Mutex<PublishedState>>,
    ) -> ControlWorker {
        let (_, commands) = sync_channel(COMMAND_CAPACITY);
        ControlWorker {
            sink,
            shared,
            shutdown: Arc::new(AtomicBool::new(false)),
            start_pending: Arc::new(AtomicBool::new(false)),
            command_gate: Arc::new(Mutex::new(())),
            cancel_start: Arc::new(AtomicBool::new(false)),
            commands,
            recorder: Recorder::default(),
            session: None,
            recording: None,
            countdown: None,
            interaction_metadata: false,
            preparation: None,
            replay_settings: ReplaySettings::default(),
            replay_only: false,
            replay_worker: None,
            storage_monitor: None,
            storage_directory: None,
            last_metrics: None,
            preview: Arc::new(Mutex::new(PreviewPublication::default())),
            idle_preview: None,
            preview_target: None,
            preview_quality: PreviewQuality::default(),
            preview_show_cursor: true,
            preview_gpu_mode: false,
            meter: Arc::new(MeterHub::default()),
            level_test: None,
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn fix007_level_error_warns_without_changing_idle_state_or_session() {
        for sequence in [41, u32::MAX] {
            let before = PublishedState {
                sequence,
                plan_decisions: vec!["Previous plan".into()],
                ..PublishedState::default()
            };
            let shared = Arc::new(Mutex::new(before.clone()));
            let sink = Arc::new(RecordingSink::default());
            let worker = fix007_worker(sink.clone(), shared.clone());
            let error = RecorderError {
                code: ErrorCode::SourceLost,
                message: "The tested audio source disconnected.".into(),
                recovery: RecoveryStatus::NoRecording,
                action: "Reconnect the source and test its levels again.".into(),
            };

            worker.publish_level_error(error.clone());

            let expected_sequence = sequence.wrapping_add(1);
            assert_eq!(
                sink.envelopes(),
                vec![EventEnvelope {
                    sequence: expected_sequence,
                    session_id: before.session_id.clone(),
                    event: EngineEvent::Warning(EngineWarning {
                        code: "audioLevelTestFailed".into(),
                        message: error.message,
                        action: error.action,
                    }),
                }]
            );
            let mut expected = before;
            expected.sequence = expected_sequence;
            assert_eq!(*shared.lock().unwrap(), expected);
            assert_eq!(worker.recorder.state(), &RecorderState::Idle);
            assert!(worker.session.is_none());
            assert!(worker.recording.is_none());
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn fix007_recording_failure_still_publishes_authoritative_failed_state() {
        let shared = Arc::new(Mutex::new(PublishedState::default()));
        let sink = Arc::new(RecordingSink::default());
        let mut worker = fix007_worker(sink.clone(), shared.clone());
        let directory = std::env::temp_dir().to_string_lossy().into_owned();
        let session = worker
            .recorder
            .begin(&request(&directory, "fix007-device-free"))
            .unwrap();
        worker.session = Some(ActiveSession::preparing(session));
        worker.publish_state(RecorderState::Preparing);
        worker.recorder.ready(session).unwrap();
        worker.publish_state(RecorderState::Recording);
        let error = RecorderError {
            code: ErrorCode::SourceLost,
            message: "The recording source disconnected.".into(),
            recovery: RecoveryStatus::Unknown,
            action: "Restore the source before starting a new recording.".into(),
        };

        worker.fail_recording(error.clone());

        assert_eq!(
            worker.recorder.state(),
            &RecorderState::Failed(error.clone())
        );
        let published = shared.lock().unwrap().clone();
        assert_eq!(published.state, RecorderState::Failed(error.clone()));
        assert_eq!(published.session_id, Some(session.to_string()));
        assert_eq!(published.sequence, 3);
        assert_eq!(
            sink.envelopes().last(),
            Some(&EventEnvelope {
                sequence: published.sequence,
                session_id: published.session_id,
                event: EngineEvent::RecorderStateChanged(RecorderState::Failed(error)),
            })
        );
        assert!(sink.artifacts().is_empty());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn recording_storage_failure_is_authoritative_and_never_publishes_a_completed_artifact() {
        for code in [ErrorCode::DiskFull, ErrorCode::Internal] {
            let shared = Arc::new(Mutex::new(PublishedState::default()));
            let sink = Arc::new(RecordingSink::default());
            let mut worker = fix007_worker(sink.clone(), shared.clone());
            let session = worker
                .recorder
                .begin(&request(
                    &std::env::temp_dir().to_string_lossy(),
                    "storage-failure",
                ))
                .unwrap();
            worker.session = Some(ActiveSession::preparing(session));
            worker.publish_state(RecorderState::Preparing);
            worker.recorder.ready(session).unwrap();
            worker.publish_state(RecorderState::Recording);
            let error = RecorderError {
                code,
                message: "Storage write failed.".into(),
                recovery: RecoveryStatus::Unknown,
                action: "Preserve incomplete files and recover a copy after freeing space.".into(),
            };
            worker.fail_recording(error.clone());
            assert_eq!(
                worker.recorder.state(),
                &RecorderState::Failed(error.clone())
            );
            assert_eq!(shared.lock().unwrap().state, RecorderState::Failed(error));
            assert!(worker.recording.is_none());
            assert!(sink.artifacts().is_empty());
        }
    }

    fn request(directory: &str, file_stem: &str) -> RecordingRequest {
        RecordingRequest {
            #[cfg(target_os = "linux")]
            target: CaptureTarget::Display(DisplayId(frame_linux::PORTAL_PICKER_ID.into())),
            #[cfg(not(target_os = "linux"))]
            target: CaptureTarget::Display(DisplayId("display-1".to_string())),
            fps_mode: FpsMode::AutoMatchSource,
            resolution: Resolution::Native,
            quality: frame_contracts::Quality::High,
            encoder: EncoderPreference::Auto,
            optimization: None,
            audio_routes: vec![],
            camera: None,
            show_cursor: true,
            output: OutputSettings {
                directory: directory.to_string(),
                file_stem: file_stem.to_string(),
            },
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn unsupported_preview_request_leaves_the_idle_owner_usable() {
        let sink = Arc::new(RecordingSink::default());
        let control = RecordingControl::new(sink);
        let error = control
            .open_preview(
                CaptureTarget::Game(frame_contracts::GameTarget {
                    process_id: 1,
                    app_identifier: None,
                }),
                PreviewQuality::Native,
                true,
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::NotImplemented);
        assert_eq!(control.published().state, RecorderState::Idle);
        assert!(control.preview_hub().is_none());
        control.close_preview().unwrap();
        control.shutdown();
    }

    #[test]
    fn whole_hertz_and_ntsc_rates_recover_their_exact_rational() {
        assert_eq!(
            nominal_rate(Some(180.0)).unwrap(),
            (NonZeroU32::new(180).unwrap(), NonZeroU32::new(1).unwrap())
        );
        assert_eq!(
            nominal_rate(Some(59.94)).unwrap(),
            (NonZeroU32::new(2997).unwrap(), NonZeroU32::new(50).unwrap())
        );
        assert_eq!(
            nominal_rate(Some(60_000.0 / 1001.0)).unwrap(),
            (
                NonZeroU32::new(60_000).unwrap(),
                NonZeroU32::new(1001).unwrap()
            )
        );
    }

    #[test]
    fn unknown_or_unusable_rates_are_reported_instead_of_invented() {
        for reported in [None, Some(0.0), Some(-60.0), Some(f64::NAN)] {
            assert_eq!(
                nominal_rate(reported).unwrap_err().code,
                ErrorCode::InvalidRequest
            );
        }
    }

    /// Telemetry reports what the live session really measured, and nothing else: the
    /// rates this host does not measure stay absent instead of becoming a plausible zero.
    #[test]
    fn telemetry_reports_only_what_was_measured() {
        let metrics = session_metrics(
            Duration::from_millis(1500),
            Some(180.0),
            Some(179.94),
            None,
            None,
            None,
        );
        assert_eq!(metrics.elapsed_ms, 1500.0);
        assert_eq!(metrics.frame_rate.requested_fps, Some(180.0));
        assert_eq!(metrics.frame_rate.source_rate_hz, Some(179.94));
        assert_eq!(metrics.frame_rate.actual_capture_fps, None);
        assert_eq!(metrics.frame_rate.encoded_fps, None);
        assert_eq!(metrics.frame_rate.dropped_frames, None);
        assert_eq!(metrics.capture_latency_ms, None);
        assert_eq!(metrics.encode_latency_ms, None);
        assert_eq!(metrics.mux_latency_ms, None);
        assert_eq!(metrics.cpu_percent, None);
        assert_eq!(metrics.gpu_percent, None);
        assert_eq!(metrics.memory_bytes, None);
        assert_eq!(metrics.av_offset_ms, None);
        assert_eq!(metrics.audio_underruns, None);
        assert_eq!(metrics.disk_bytes_per_second, None);
        assert!(metrics.queues.is_empty());

        // Once a real sample exists it is reported as the measurement it is.
        let measured = session_metrics(
            Duration::from_secs(2),
            Some(180.0),
            Some(180.0),
            Some(178.5),
            Some(176.0),
            None,
        );
        assert_eq!(measured.frame_rate.actual_capture_fps, Some(178.5));
        assert_eq!(measured.frame_rate.encoded_fps, Some(176.0));
    }

    #[cfg(target_os = "windows")]
    #[cfg(target_os = "windows")]
    #[test]
    fn video_pipeline_counts_keep_source_capture_submit_encode_and_loss_distinct() {
        let capture = frame_capture::CapturePipelineStats {
            source_presentations: 240,
            captured_frames: 203,
            captured_unique_frames: 200,
            pool_pressure_drops: 25,
            ready_queue_drops: 10,
            contention_drops: 2,
            upstream_accumulated_updates: 19,
            ready_depth: Some(1),
            ready_capacity: 2,
        };
        let encoder = frame_windows::EncoderSessionStats {
            accepted_inputs: 51,
            completed_frames: 48,
            packets_emitted: 48,
            packets_held: 3,
            ingress_capacity: 3,
            packet_capacity: 4,
            ..Default::default()
        };
        // Owner totals survive a segment replacement; native counters belong
        // only to the current encoder. Cursor images count separately too.
        let counts = native_encoder_counters(190, 180, 172, 7, Some(encoder), Some(capture), 13);
        assert_eq!(counts.capture_frames, Some(200));
        assert_eq!(counts.source_frames, Some(240));
        assert_eq!(counts.submitted_frames, Some(180));
        assert_eq!(counts.completed_frames, Some(175));
        let c = counts.pipeline.unwrap();
        assert_eq!(c.captured_frames, 203);
        assert_eq!(c.captured_unique_frames, 200);
        assert_eq!(c.policy_skipped_frames, 13);
        assert_eq!(c.backpressure_events, 7);
        assert_eq!(
            c.pool_pressure_drops + c.ready_queue_drops + c.contention_drops,
            37
        );
        assert_eq!(c.upstream_accumulated_updates, 19);
        assert_eq!(counts.capture_queue, Some((1, 2, 10)));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn encoder_queue_stats_map_to_two_live_queue_gauges() {
        let stats = frame_windows::EncoderSessionStats {
            queued_inputs: 2,
            ingress_capacity: 3,
            packets_held: 3,
            packet_capacity: 4,
            ..Default::default()
        };
        let metrics = session_metrics(
            Duration::from_secs(1),
            Some(60.0),
            Some(60.0),
            None,
            None,
            Some(EncoderQueueGauges::from_encoder(stats)),
        );

        assert_eq!(
            metrics.queues,
            vec![
                QueueMetrics {
                    name: "Encoder ingress".to_string(),
                    depth: 2,
                    capacity: 3,
                    dropped: 0,
                },
                QueueMetrics {
                    name: "Encoded packet delivery".to_string(),
                    depth: 3,
                    capacity: 4,
                    dropped: 0,
                },
            ]
        );
    }

    #[test]
    fn missing_encoder_stats_publish_no_queue_gauges() {
        let metrics = session_metrics(
            Duration::from_secs(1),
            Some(60.0),
            Some(60.0),
            None,
            None,
            None,
        );

        assert!(metrics.queues.is_empty());
    }

    #[test]
    fn new_session_queue_gauges_do_not_reuse_previous_values() {
        let previous = session_metrics(
            Duration::from_secs(5),
            Some(60.0),
            Some(60.0),
            Some(59.0),
            Some(58.0),
            Some(EncoderQueueGauges {
                ingress_depth: 3,
                ingress_capacity: 3,
                packet_depth: 4,
                packet_capacity: 4,
            }),
        );
        assert_eq!(previous.queues.len(), 2);

        let next = session_metrics(Duration::ZERO, Some(60.0), Some(60.0), None, None, None);
        assert!(next.queues.is_empty());
        assert_eq!(next.frame_rate.actual_capture_fps, None);
        assert_eq!(next.frame_rate.encoded_fps, None);
    }

    /// The telemetry cadence is a bound, not a queue: no event is published inside the
    /// interval, and a long gap is one event instead of a catch-up burst.
    #[test]
    fn telemetry_is_published_at_most_four_times_per_second() {
        let start = Instant::now();
        assert!(metrics_due(None, start), "the first observation is due");
        assert!(!metrics_due(
            Some(start),
            start + Duration::from_millis(249)
        ));
        assert!(metrics_due(Some(start), start + Duration::from_millis(250)));
        assert!(metrics_due(Some(start), start + Duration::from_secs(3)));
    }

    #[test]
    fn scope_accepts_planned_routes_and_bounded_window_targets() {
        let directory = std::env::temp_dir();
        let directory = directory.to_string_lossy().into_owned();
        assert!(validate_scope(&request(&directory, "scope-ok")).is_ok());

        let mut fixed = request(&directory, "scope-fixed");
        fixed.fps_mode = FpsMode::Fixed(240.0);
        assert_eq!(
            validate_scope(&fixed).is_ok(),
            !cfg!(any(target_os = "linux", target_os = "macos"))
        );

        let mut scaled = request(&directory, "scope-scaled");
        scaled.resolution = Resolution::Exact(frame_contracts::Dimensions {
            width: 1920,
            height: 1080,
        });
        assert_eq!(validate_scope(&scaled).is_ok(), !cfg!(target_os = "linux"));

        let mut window = request(&directory, "scope-window");
        window.target = CaptureTarget::Window(frame_contracts::WindowId("window-1".to_string()));
        assert!(window.show_cursor);
        assert_eq!(validate_scope(&window).is_ok(), !cfg!(target_os = "linux"));
        window.show_cursor = false;
        assert_eq!(validate_scope(&window).is_ok(), !cfg!(target_os = "linux"));
        window.fps_mode = FpsMode::Fixed(480.0);
        window.encoder = EncoderPreference::Explicit("nvidia-nvenc-h264-d3d11:luid=1".into());
        assert_eq!(
            validate_scope(&window).is_ok(),
            !cfg!(any(target_os = "linux", target_os = "macos"))
        );
        window.fps_mode = FpsMode::AutoMatchSource;
        window.resolution = Resolution::Exact(frame_contracts::Dimensions {
            width: 1280,
            height: 720,
        });
        // The planner checks exact geometry against the actual native window.
        assert_eq!(validate_scope(&window).is_ok(), !cfg!(target_os = "linux"));
        window.resolution = Resolution::Native;
        window.target = CaptureTarget::Game(frame_contracts::GameTarget {
            process_id: 17,
            app_identifier: Some("window-1".into()),
        });
        assert_eq!(validate_scope(&window).is_ok(), cfg!(target_os = "windows"));

        #[cfg(target_os = "linux")]
        {
            let mut named_display = request(&directory, "scope-named-display");
            named_display.target = CaptureTarget::Display(DisplayId("display-1".into()));
            assert_eq!(
                validate_scope(&named_display).unwrap_err().code,
                ErrorCode::NotImplemented
            );
            let mut explicit_encoder = request(&directory, "scope-explicit-encoder");
            explicit_encoder.encoder = EncoderPreference::Explicit("vaapi-h264".into());
            assert_eq!(
                validate_scope(&explicit_encoder).unwrap_err().code,
                ErrorCode::NotImplemented
            );
        }

        let mut routed = request(&directory, "scope-audio");
        routed.audio_routes = vec![frame_contracts::AudioRouteSelection {
            controls: Default::default(),
            muted: false,
            source_id: frame_contracts::AudioSourceId("microphone-1".to_string()),
            route: AudioRoute::MainMix,
        }];
        assert!(validate_scope(&routed).is_ok());

        // An ignored route is the video-only request this host does implement.
        let mut ignored = request(&directory, "scope-ignored");
        ignored.audio_routes = vec![frame_contracts::AudioRouteSelection {
            controls: Default::default(),
            muted: false,
            source_id: frame_contracts::AudioSourceId("microphone-1".to_string()),
            route: AudioRoute::Ignore,
        }];
        assert!(validate_scope(&ignored).is_ok());
    }

    #[test]
    fn selected_routes_choose_silent_or_real_main_and_clean_tracks() {
        use frame_audio::TrackKind;
        let directory = std::env::temp_dir().to_string_lossy().into_owned();
        let mut selection = request(&directory, "routes");
        let routes = [
            AudioRoute::Ignore,
            AudioRoute::MainMix,
            AudioRoute::SeparateOnly,
            AudioRoute::MainMixAndSeparate,
        ];
        selection.audio_routes = routes
            .into_iter()
            .enumerate()
            .map(|(index, route)| AudioRouteSelection {
                controls: Default::default(),
                muted: false,
                source_id: AudioSourceId(format!("source-{index}")),
                route,
            })
            .collect();
        let selected = selected_routes(&selection);
        assert_eq!(selected.len(), 3);
        assert!(
            selected
                .iter()
                .all(|route| route.route != AudioRoute::Ignore)
        );
        let tracks = frame_audio::plan_tracks(&selected).unwrap();
        assert_eq!(tracks.len(), 3);
        assert_eq!(tracks[0].kind, TrackKind::MainMix);
        assert_eq!(
            tracks[0].sources,
            vec![
                AudioSourceId("source-1".into()),
                AudioSourceId("source-3".into())
            ]
        );
        assert_eq!(
            tracks[1].kind,
            TrackKind::Clean(AudioSourceId("source-2".into()))
        );
        assert_eq!(
            tracks[2].kind,
            TrackKind::Clean(AudioSourceId("source-3".into()))
        );
        selection
            .audio_routes
            .iter_mut()
            .for_each(|route| route.route = AudioRoute::Ignore);
        assert!(selected_routes(&selection).is_empty());
    }

    #[test]
    fn source_refresh_keeps_routes_and_sequence_in_one_snapshot() {
        let control = RecordingControl::new(Arc::new(RecordingSink::default()));
        let source = AudioSource {
            controls: Default::default(),
            capture_identity: None,
            muted: false,
            id: AudioSourceId("microphone".into()),
            kind: frame_contracts::AudioSourceKind::Microphone,
            display_name: "Test microphone".into(),
            process_id: None,
            app_identifier: None,
            is_active: true,
            route: AudioRoute::Ignore,
        };
        let before = control.observe_sources(vec![source.clone()]);
        {
            let mut shared = control.shared.lock().unwrap();
            shared
                .saved_routes
                .insert(source.id.clone(), AudioRoute::MainMix);
            shared.sequence += 1;
        }
        let after = control.observe_sources(vec![source]);
        assert_eq!(before.sequence, 0);
        assert_eq!(before.audio_sources[0].route, AudioRoute::Ignore);
        assert_eq!(after.sequence, 1);
        assert_eq!(after.audio_sources[0].route, AudioRoute::MainMix);
        control.shutdown();
    }

    #[test]
    fn queued_route_choice_wins_over_a_stale_window_request() {
        let directory = std::env::temp_dir().to_string_lossy().into_owned();
        let mut request = request(&directory, "queued-route");
        request.audio_routes.push(AudioRouteSelection {
            controls: Default::default(),
            muted: false,
            source_id: AudioSourceId("microphone".into()),
            route: AudioRoute::Ignore,
        });
        let saved = BTreeMap::from([(
            AudioSourceId("microphone".into()),
            AudioRoute::MainMixAndSeparate,
        )]);
        apply_saved_routes(&mut request, &saved);
        assert_eq!(
            selected_routes(&request)[0].route,
            AudioRoute::MainMixAndSeparate
        );
    }

    #[test]
    fn an_existing_file_relative_path_or_missing_directory_is_refused() {
        let directory = std::env::temp_dir();
        let directory_text = directory.to_string_lossy().into_owned();
        let existing = directory.join("frame-0111b-existing.mkv");
        std::fs::write(&existing, b"evidence").unwrap();
        let error = output_path(&request(
            &directory_text,
            existing.file_stem().unwrap().to_str().unwrap(),
        ))
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidOutputPath);
        std::fs::remove_file(&existing).unwrap();

        assert_eq!(
            output_path(&request("relative/directory", "name"))
                .unwrap_err()
                .code,
            ErrorCode::InvalidOutputPath
        );
        let missing = directory.join("frame-0111b-missing-directory");
        assert_eq!(
            output_path(&request(&missing.to_string_lossy(), "name"))
                .unwrap_err()
                .code,
            ErrorCode::InvalidOutputPath
        );
        assert_eq!(
            output_path(&request(&directory_text, "../escape"))
                .unwrap_err()
                .code,
            ErrorCode::InvalidOutputPath
        );
        assert_eq!(
            output_path(&request(&directory_text, "plain-name")).unwrap(),
            directory.join("plain-name.mkv")
        );
    }

    #[test]
    fn automatic_names_use_a_fresh_suffix_without_touching_existing_files() {
        let directory = std::env::temp_dir().join(format!(
            "freym-auto-name-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let first = auto_output_path(&directory, "2026-09-28_14-00-00").unwrap();
        assert_eq!(first, directory.join("FREYM_2026-09-28_14-00-00.mkv"));
        std::fs::write(&first, b"original").unwrap();
        let second = auto_output_path(&directory, "2026-09-28_14-00-00").unwrap();
        assert_eq!(second, directory.join("FREYM_2026-09-28_14-00-00_2.mkv"));
        std::fs::write(&second, b"second").unwrap();
        let third = auto_output_path(&directory, "2026-09-28_14-00-00").unwrap();
        assert_eq!(third, directory.join("FREYM_2026-09-28_14-00-00_3.mkv"));
        assert_eq!(std::fs::read(&first).unwrap(), b"original");
        assert_eq!(std::fs::read(&second).unwrap(), b"second");
        let request = request(&directory.to_string_lossy(), AUTO_FILE_STEM);
        let next_recording = output_path(&request).unwrap();
        assert!(next_recording.starts_with(&directory));
        assert!(
            next_recording
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("FREYM_")
        );
        assert!(!next_recording.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn countdown_reserves_owner_and_stop_cancels_before_native_start() {
        let directory = std::env::temp_dir();
        let output = directory.join("frame-0306-countdown-cancel.mkv");
        let sink = Arc::new(RecordingSink::default());
        let control = RecordingControl::new_inner(
            sink.clone(),
            #[cfg(target_os = "windows")]
            Some(Box::new(|_| {
                panic!("countdown cancellation must not prepare native owners")
            })),
        );
        let pending = request(&directory.to_string_lossy(), "frame-0306-countdown-cancel");
        #[cfg(not(target_os = "linux"))]
        let pending = RecordingRequest {
            target: CaptureTarget::Display(DisplayId("definitely-not-attached".into())),
            ..pending
        };
        control.start_with_countdown(pending.clone(), 3).unwrap();
        assert_eq!(
            control.start_with_countdown(pending, 3).unwrap_err().code,
            ErrorCode::InvalidTransition
        );
        control.stop().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while control.published().sequence < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(control.published().state, RecorderState::Idle);
        assert!(sink.states().contains(&RecorderState::Preparing));
        assert!(sink.artifacts().is_empty());
        assert!(!output.exists());
        control.shutdown();
    }

    /// The one focused teardown scenario: a native start that fails keeps its own code,
    /// releases everything and produces no artifact, and the host returns to idle.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_native_start_failure_reports_its_code_and_releases_everything() {
        let directory = std::env::temp_dir();
        let output = directory.join("frame-0111b-native-start-failure.mkv");
        let _ = std::fs::remove_file(&output);

        let sink = Arc::new(RecordingSink::default());
        let control = RecordingControl::new(sink.clone());
        let mut unattached = request(
            &directory.to_string_lossy(),
            "frame-0111b-native-start-failure",
        );
        unattached.target =
            CaptureTarget::Display(DisplayId("frame-0111b-unattached-display".to_string()));
        control.start(unattached.clone()).unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let failed = loop {
            if let RecorderState::Failed(error) = control.published().state {
                break error;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the native start failure was not reported: {:?}",
                sink.states()
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        // The native error reaches the interface with its own code, not a generic one.
        assert_eq!(failed.code, ErrorCode::SourceLost);
        assert!(!failed.message.is_empty() && !failed.action.is_empty());
        // Nothing was finalized and nothing was written.
        assert!(sink.artifacts().is_empty());
        assert!(!output.exists());
        // A failed session publishes no result and no telemetry, so the interface cannot
        // present it as a completed recording.
        let published = control.published();
        assert!(published.last_artifact.is_none());
        assert!(published.metrics.is_none());
        // Preparing was published before the native work, so a caller never saw idle.
        assert_eq!(
            sink.states(),
            vec![
                RecorderState::Preparing,
                RecorderState::Failed(failed.clone())
            ]
        );

        control.reset().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while control.published().state != RecorderState::Idle {
            assert!(
                std::time::Instant::now() < deadline,
                "the reset did not return to idle"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // The published sequence and the event stream stayed in step.
        let published = control.published();
        let last = sink.envelopes().last().cloned().unwrap();
        assert_eq!(published.sequence, last.sequence);
        assert_eq!(published.session_id, last.session_id);
        assert_eq!(
            EngineEvent::RecorderStateChanged(published.state.clone()),
            last.event
        );
        // Reset releases the failed session's admission gate. A second start is
        // accepted as a new session, and another native failure still produces no
        // successful artifact or output file.
        let first_session = sink.envelopes().iter().find_map(|envelope| {
            matches!(
                envelope.event,
                EngineEvent::RecorderStateChanged(RecorderState::Preparing)
            )
            .then(|| envelope.session_id.clone())
            .flatten()
        });
        control.start(unattached).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !matches!(control.published().state, RecorderState::Failed(_)) {
            assert!(
                Instant::now() < deadline,
                "the second start did not fail cleanly"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(control.published().session_id, first_session);
        assert!(sink.artifacts().is_empty());
        assert!(!output.exists());
        control.shutdown();
    }
}
