//! Native facts supplied by frame-runtime. Missing evidence is never support.
use crate::{EncoderCandidate, model::RecordingConfiguration};
use frame_contracts::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperatingSystem {
    Windows,
    MacOs,
    Linux,
    Other,
}
impl OperatingSystem {
    pub fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Other
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuCapability {
    pub id: String,
    pub name: String,
    pub vendor_id: u32,
    pub driver_version: Option<u64>,
}
#[derive(Clone, Debug)]
pub enum AudioDeviceQualification {
    /// Known input format accepted by the native converter/normalizer.
    Format {
        sample_rate_hz: u32,
        channels: u16,
        sample_encoding: String,
    },
    /// A live application process/permission still requires actual capture activation.
    AtStart,
    Unavailable(String),
}
/// Qualification is scoped to these inputs. Changing them requires a fresh probe.
#[derive(Clone, Debug, PartialEq)]
pub struct VideoProbeKey {
    pub target: CaptureTarget,
    pub fps_mode: FpsMode,
    pub resolution: Resolution,
    pub quality: Quality,
    pub encoder: EncoderPreference,
    pub optimization: Option<RecordingOptimization>,
    pub camera: Option<CameraSelection>,
}
impl From<&RecordingConfiguration> for VideoProbeKey {
    fn from(c: &RecordingConfiguration) -> Self {
        let r = &c.request;
        Self {
            target: r.target.clone(),
            fps_mode: r.fps_mode,
            resolution: r.resolution,
            quality: r.quality,
            encoder: r.encoder.clone(),
            optimization: r.optimization.clone(),
            camera: r.camera.clone(),
        }
    }
}
#[derive(Clone, Debug)]
pub struct VideoCapabilities {
    /// Exact native initialization evidence for the current whole override set.
    pub advanced_qualification: Option<(String, VideoEncoderOverrides)>,
    pub key: VideoProbeKey,
    pub capture_gpu_id: Option<String>,
    pub native: Dimensions,
    /// Encoder configuration rate, not measured capture/encode throughput.
    pub nominal_rate: crate::NominalRate,
    pub hdr_source: bool,
    pub default_codec: String,
    pub candidates: Vec<EncoderCandidate>,
}
#[derive(Clone, Debug)]
pub struct SettingsCapabilities {
    pub os: OperatingSystem,
    /// Populated by RtlGetVersion on Windows. Other platforms do not guess a version.
    pub os_build: Option<u32>,
    pub engine: EngineCapabilities,
    pub displays: Vec<DisplayInfo>,
    pub windows: Vec<WindowInfo>,
    pub audio_sources: Vec<AudioSource>,
    pub audio_devices: Vec<(AudioSourceId, AudioDeviceQualification)>,
    pub cameras: Vec<(CameraInfo, Vec<CameraFormat>)>,
    pub gpus: Vec<GpuCapability>,
    /// None means unqualified, including platforms whose geometry is chosen at Start.
    pub video: Option<VideoCapabilities>,
    pub diagnostics: Vec<String>,
}
impl SettingsCapabilities {
    pub fn from_snapshot(snapshot: EngineSnapshot) -> Self {
        Self {
            os: OperatingSystem::current(),
            os_build: None,
            engine: snapshot.capabilities,
            displays: snapshot.displays,
            windows: snapshot.windows,
            audio_sources: snapshot.audio_sources,
            audio_devices: Vec::new(),
            cameras: Vec::new(),
            gpus: Vec::new(),
            video: None,
            diagnostics: Vec::new(),
        }
    }
}
