//! Canonical wire contracts. No OS handles, UI dependencies or media processing.
use serde::{Deserialize, Serialize};

macro_rules! id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
        pub struct $name(pub String);
    };
}
id!(DisplayId);
id!(WindowId);
id!(AudioSourceId);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecorderError {
    pub code: ErrorCode,
    pub message: String,
    pub recovery: RecoveryStatus,
    pub action: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorCode {
    NotImplemented,
    InvalidRequest,
    InvalidTransition,
    PermissionDenied,
    SourceLost,
    EncoderUnavailable,
    DiskFull,
    InvalidOutputPath,
    UnsupportedCodec,
    FinalizationFailed,
    Internal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RecoveryStatus {
    NoRecording,
    Recoverable,
    Unknown,
}

impl RecorderError {
    pub fn unsupported(operation: &str) -> Self {
        Self {
            code: ErrorCode::NotImplemented,
            message: format!("{operation} is not implemented."),
            recovery: RecoveryStatus::NoRecording,
            action: "Choose a supported source or configuration.".into(),
        }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::InvalidRequest,
            message: message.into(),
            recovery: RecoveryStatus::NoRecording,
            action: "Review recording settings and try again.".into(),
        }
    }
}

impl std::fmt::Display for RecorderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for RecorderError {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "error", rename_all = "camelCase")]
pub enum RecorderState {
    Idle,
    Preparing,
    Recording,
    Paused,
    Finalizing,
    Failed(RecorderError),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Region {
    pub display_id: DisplayId,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameTarget {
    pub process_id: u32,
    pub app_identifier: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "camelCase")]
pub enum CaptureTarget {
    Display(DisplayId),
    Window(WindowId),
    Region(Region),
    Game(GameTarget),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "camelCase")]
pub enum FpsMode {
    AutoMatchSource,
    Fixed(f64),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DisplayInfo {
    pub id: DisplayId,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
    pub scale_factor: f64,
    pub refresh_rate_hz: Option<f64>,
    pub is_primary: bool,
    /// Capture source HDR availability; encoder/HDR recording and replay export
    /// are independently admitted and must never be inferred from this flag.
    pub hdr_available: bool,
    /// A known mandatory tier for this source's current recording policy.
    /// Absence does not assert encoder availability; Start revalidates admission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_recording_quality: Option<Quality>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowInfo {
    pub id: WindowId,
    pub title: String,
    pub process_id: Option<u32>,
    /// Executable basename for display only; the canonical window identity remains HWND + PID.
    #[serde(default)]
    pub process_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AudioRoute {
    Ignore,
    MainMix,
    SeparateOnly,
    MainMixAndSeparate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AudioSourceKind {
    Application,
    Microphone,
    System,
}

/// Per-source recording controls. Integer units make persisted values exact.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct AudioSourceControls {
    pub gain_millidb: i32,
    /// Stereo balance: -100 left, 0 center, 100 right. Mono stems stay mono.
    pub balance: i16,
    pub reconnect: AudioReconnectPolicy,
}
impl AudioSourceControls {
    pub fn validate(&self) -> Result<(), RecorderError> {
        if !(-60_000..=24_000).contains(&self.gain_millidb) || !(-100..=100).contains(&self.balance)
        {
            return Err(RecorderError::invalid(
                "Audio gain must be -60…+24 dB and balance -100…100.",
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AudioReconnectPolicy {
    /// Preserve the existing process/environment default for older documents.
    #[default]
    ProcessDefault,
    Silence,
    ExactDevice,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioCaptureIdentity {
    pub endpoint: Option<String>,
    pub process_instance: Option<String>,
    pub ancestor_instances: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioSource {
    #[serde(default)]
    pub controls: AudioSourceControls,
    /// Native observation only; process lifetime tokens prevent PID-reuse aliases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_identity: Option<AudioCaptureIdentity>,
    /// Silences this source without changing its capture route or track identity.
    #[serde(default)]
    pub muted: bool,
    pub id: AudioSourceId,
    pub kind: AudioSourceKind,
    pub display_name: String,
    pub process_id: Option<u32>,
    pub app_identifier: Option<String>,
    pub is_active: bool,
    pub route: AudioRoute,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioRouteSelection {
    #[serde(default)]
    pub controls: AudioSourceControls,
    /// Silences this source without changing its capture route or track identity.
    #[serde(default)]
    pub muted: bool,
    pub source_id: AudioSourceId,
    pub route: AudioRoute,
}

/// A real unmixed PCM measurement. Null fields mean no block was measured yet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioLevel {
    pub source_id: AudioSourceId,
    pub rms: Option<f32>,
    pub peak: Option<f32>,
    pub clipping: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioLevelsSnapshot {
    pub session_id: Option<String>,
    pub sources: Vec<AudioLevel>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraFormat {
    pub index: u32,
    pub width: u32,
    pub height: u32,
    pub frame_rate_num: u32,
    pub frame_rate_den: u32,
    pub subtype: String,
    pub supported: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraPlacement {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraSelection {
    pub device_id: String,
    pub format_index: u32,
    pub placement: CameraPlacement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "camelCase")]
pub enum Resolution {
    Native,
    Exact(Dimensions),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dimensions {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Quality {
    Standard,
    High,
    Maximum,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "camelCase")]
pub enum EncoderPreference {
    Auto,
    Explicit(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RecordingPreset {
    Gaming,
    #[default]
    Desktop,
    Lossless,
    Custom,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RecordingCodec {
    #[default]
    Auto,
    H264,
    Hevc,
    Av1,
}

/// Explicit native encoder intent. None preserves the automatic recording policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VideoRateControl {
    Cbr,
    Vbr,
    Cqp,
    Icq,
    Crf,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VideoEncoderPreset {
    P1,
    P2,
    P3,
    P4,
    P5,
    P6,
    P7,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VideoEncoderTuning {
    UltraLowLatency,
    LowLatency,
    HighQuality,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VideoMultipass {
    Disabled,
    QuarterResolution,
    FullResolution,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct VideoEncoderOverrides {
    pub rate_control: Option<VideoRateControl>,
    /// Equal I/P/B QP; bounded to 0..51 on the implemented SDR NVENC path.
    pub qp: Option<u32>,
    pub preset: Option<VideoEncoderPreset>,
    pub tuning: Option<VideoEncoderTuning>,
    pub gop_frames: Option<u32>,
    pub b_frames: Option<u32>,
    pub multipass: Option<VideoMultipass>,
    pub lookahead_frames: Option<u32>,
    pub spatial_aq: Option<bool>,
    pub temporal_aq: Option<bool>,
}
impl VideoEncoderOverrides {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
    /// Path restrictions are intentional: buffering/reordering is not implemented.
    pub fn validate(&self) -> Result<(), RecorderError> {
        if matches!(
            self.rate_control,
            Some(VideoRateControl::Icq | VideoRateControl::Crf)
        ) {
            return Err(RecorderError::invalid(
                "ICQ and CRF are unavailable on the active native encoder path; no substitution is allowed.",
            ));
        }
        let cqp = self.rate_control == Some(VideoRateControl::Cqp);
        if cqp != self.qp.is_some() || self.qp.is_some_and(|q| q > 51) {
            return Err(RecorderError::invalid(
                "CQP requires an explicit QP in 0..51; QP is unavailable in other modes.",
            ));
        }
        if self.gop_frames == Some(0) || self.gop_frames == Some(u32::MAX) {
            return Err(RecorderError::invalid(
                "GOP must be a finite positive frame interval.",
            ));
        }
        if self.b_frames.is_some_and(|n| n != 0) || self.lookahead_frames.is_some_and(|n| n != 0) {
            return Err(RecorderError::invalid(
                "This four-surface recording path does not support B-frame reordering or encoder lookahead.",
            ));
        }
        if cqp
            && (self
                .multipass
                .is_some_and(|v| v != VideoMultipass::Disabled)
                || self.spatial_aq == Some(true)
                || self.temporal_aq == Some(true))
        {
            return Err(RecorderError::invalid(
                "CQP requires multipass and adaptive quantization disabled to preserve constant QP.",
            ));
        }
        Ok(())
    }
}

/// Optional so older settings retain their exact manual selections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RecordingOptimization {
    pub preset: RecordingPreset,
    pub codec: RecordingCodec,
    pub automatic_quality: bool,
    pub bitrate_bps: Option<u32>,
    pub encoder_overrides: VideoEncoderOverrides,
}

impl Default for RecordingOptimization {
    fn default() -> Self {
        Self {
            preset: RecordingPreset::Desktop,
            codec: RecordingCodec::Auto,
            automatic_quality: true,
            bitrate_bps: None,
            encoder_overrides: Default::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputSettings {
    pub directory: String,
    pub file_stem: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingRequest {
    pub target: CaptureTarget,
    pub fps_mode: FpsMode,
    pub resolution: Resolution,
    pub quality: Quality,
    pub encoder: EncoderPreference,
    #[serde(default)]
    pub optimization: Option<RecordingOptimization>,
    pub audio_routes: Vec<AudioRouteSelection>,
    #[serde(default)]
    pub camera: Option<CameraSelection>,
    #[serde(default = "default_show_cursor")]
    pub show_cursor: bool,
    pub output: OutputSettings,
}

pub fn default_show_cursor() -> bool {
    true
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameRateStatus {
    pub requested_fps: Option<f64>,
    pub source_rate_hz: Option<f64>,
    pub actual_capture_fps: Option<f64>,
    pub encoded_fps: Option<f64>,
    /// Observed native presentation identities, excluding cursor-only updates.
    /// This does not assert pixel-distinct content or unseen application frames.
    #[serde(default)]
    pub observed_source_fps: Option<f64>,
    #[serde(default)]
    pub submitted_fps: Option<f64>,
    #[serde(default)]
    pub counters: Option<VideoFrameCounters>,
    /// Dropped frames, or `null` when this host measures no drop counter. It is never
    /// published as `0` for a pipeline that does not count drops, because `0` claims a
    /// measurement that was not made.
    pub dropped_frames: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoFrameCounters {
    pub source_presentations: u64,
    pub captured_frames: u64,
    /// Distinct native presentation identities copied, excluding cursor-only
    /// visual updates. Pixel uniqueness is not inferred from timestamps.
    pub captured_unique_frames: u64,
    pub submitted_frames: u64,
    pub encoded_frames: u64,
    pub policy_skipped_frames: u64,
    pub backpressure_events: u64,
    pub pool_pressure_drops: u64,
    pub ready_queue_drops: u64,
    pub contention_drops: u64,
    /// Estimates reported by DXGI; excluded from recording drop totals.
    pub upstream_accumulated_updates: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueMetrics {
    pub name: String,
    pub depth: u32,
    pub capacity: u32,
    pub dropped: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecorderMetrics {
    pub frame_rate: FrameRateStatus,
    pub elapsed_ms: f64,
    pub capture_latency_ms: Option<f64>,
    pub encode_latency_ms: Option<f64>,
    pub mux_latency_ms: Option<f64>,
    pub queues: Vec<QueueMetrics>,
    pub cpu_percent: Option<f64>,
    pub gpu_percent: Option<f64>,
    pub memory_bytes: Option<f64>,
    pub av_offset_ms: Option<f64>,
    pub audio_underruns: Option<u32>,
    pub disk_bytes_per_second: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineWarning {
    pub code: String,
    pub message: String,
    pub action: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingArtifact {
    pub path: String,
    pub duration_ms: f64,
    pub audio_tracks: u32,
}

/// Restart discovery candidates; the marker alone does not establish recoverability.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InterruptedRecordingScan {
    pub paths: Vec<String>,
    pub truncated: bool,
}

/// A recovered file is a preserved prefix, not a successful recording finalization.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingRecoveryResult {
    pub source_path: String,
    pub output_path: String,
    pub duration_ms: f64,
    pub recovered_packets: u64,
    pub unparsed_tail_bytes: u64,
    pub final_trim_known: bool,
    pub tail_reason: Option<String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplaySettings {
    pub duration_seconds: u32,
    pub max_bytes: u32,
    #[serde(default = "default_replay_storage_mib")]
    pub storage_mib: u32,
}

fn default_replay_storage_mib() -> u32 {
    512
}

impl Default for ReplaySettings {
    fn default() -> Self {
        Self {
            duration_seconds: 30,
            max_bytes: 64 * 1024 * 1024,
            storage_mib: default_replay_storage_mib(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplaySaveResult {
    pub artifact: RecordingArtifact,
    pub requested_seconds: u32,
    pub retained_bytes: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShortcutBindings {
    pub start_stop: String,
    pub pause_resume: String,
    pub save_replay: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineCapabilities {
    pub can_record: bool,
    pub can_pause: bool,
    pub can_replay: bool,
    pub can_hotkeys: bool,
    #[serde(default)]
    pub camera_overlay: bool,
    #[serde(default)]
    pub live_preview: bool,
    #[serde(default)]
    pub audio_levels: bool,
    #[serde(default)]
    pub fixed_fps: bool,
    pub display_capture: bool,
    pub window_capture: bool,
    pub region_capture: bool,
    pub game_capture: bool,
    pub per_app_audio: bool,
    pub microphone: bool,
    pub reason: Option<String>,
}

impl EngineCapabilities {
    pub fn unavailable() -> Self {
        Self {
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
            reason: Some("Recording capabilities are unavailable.".into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "camelCase")]
pub enum EngineEvent {
    RecorderStateChanged(RecorderState),
    DisplaysChanged(Vec<DisplayInfo>),
    WindowsChanged(Vec<WindowInfo>),
    AudioSourcesChanged(Vec<AudioSource>),
    Metrics(Box<RecorderMetrics>),
    /// The frozen decisions the native session is preparing with.
    PlanDecisions(Vec<String>),
    Warning(EngineWarning),
    Error(RecorderError),
    RecordingFinalized(RecordingArtifact),
    ReplaySaved(ReplaySaveResult),
    ReplaySaveFailed(RecorderError),
    ReplayConfigured(ReplaySettings),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventEnvelope {
    pub sequence: u32,
    pub session_id: Option<String>,
    pub event: EngineEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineSnapshot {
    pub sequence: u32,
    pub session_id: Option<String>,
    pub state: RecorderState,
    pub capabilities: EngineCapabilities,
    pub displays: Vec<DisplayInfo>,
    pub windows: Vec<WindowInfo>,
    pub audio_sources: Vec<AudioSource>,
    pub metrics: Option<RecorderMetrics>,
    /// The frozen plan for the active or most recently completed session.
    pub plan_decisions: Vec<String>,
    /// The last recording this host really finalized, or `null` when none has completed
    /// in this process. Only a successful finalization fills it: a failure, an abort and
    /// a shutdown never replace it with a success, and a new session clears it when that
    /// session starts preparing. It survives a snapshot refresh because it is part of the
    /// published host state, not a transient event.
    pub last_artifact: Option<RecordingArtifact>,
    pub replay_settings: ReplaySettings,
    #[serde(default)]
    pub replay_active: bool,
    #[serde(default)]
    pub replay_only: bool,
    pub last_replay: Option<ReplaySaveResult>,
    pub warnings: Vec<EngineWarning>,
}

impl Default for EngineSnapshot {
    fn default() -> Self {
        Self {
            sequence: 0,
            session_id: None,
            state: RecorderState::Idle,
            capabilities: EngineCapabilities::unavailable(),
            displays: vec![],
            windows: vec![],
            audio_sources: vec![],
            metrics: None,
            plan_decisions: vec![],
            last_artifact: None,
            replay_settings: ReplaySettings::default(),
            replay_active: false,
            replay_only: false,
            last_replay: None,
            warnings: vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn display_quality_requirement_is_optional_on_the_wire() {
        let legacy = serde_json::json!({
            "id": "display-1", "name": "test panel", "width": 1920, "height": 1080,
            "x": 0, "y": 0, "scaleFactor": 1.0, "refreshRateHz": 60.0,
            "isPrimary": true, "hdrAvailable": false
        });
        let mut display: DisplayInfo = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(display.required_recording_quality, None);
        assert_eq!(serde_json::to_value(&display).unwrap(), legacy);
        display.required_recording_quality = Some(Quality::Standard);
        let constrained = serde_json::to_value(&display).unwrap();
        assert_eq!(constrained["requiredRecordingQuality"], "standard");
        assert_eq!(
            serde_json::from_value::<DisplayInfo>(constrained).unwrap(),
            display
        );
    }

    #[test]
    fn wire_discriminants_are_stable() {
        assert_eq!(
            serde_json::to_value(FpsMode::Fixed(480.0)).unwrap(),
            serde_json::json!({"kind":"fixed", "value":480.0})
        );
        assert_eq!(
            serde_json::to_value(RecorderState::Idle).unwrap(),
            serde_json::json!({"kind":"idle"})
        );
        let event = EngineEvent::RecorderStateChanged(RecorderState::Failed(
            RecorderError::unsupported("Capture"),
        ));
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(serde_json::from_str::<EngineEvent>(&json).unwrap(), event);
    }
}
