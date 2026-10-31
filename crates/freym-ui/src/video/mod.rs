//! Video presentation and the narrow adapter to the existing recording request.
pub(crate) mod adapter;

use adapter::{VideoCapabilities, VideoContext};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum VideoMode {
    #[default]
    Easy,
    Advanced,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VideoSection {
    Geometry,
    Encoder,
    Rate,
    Preset,
    Analysis,
    Color,
    Capture,
}
impl VideoSection {
    pub fn index(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum VideoEdit {
    Resolution(frame_contracts::Dimensions),
    NativeResolution,
    EncoderOverrides(frame_contracts::VideoEncoderOverrides),
    // Millihertz keeps menu actions exact and comparable; the custom editor retains f64.
    Fps(Option<u32>),
    Quality(Option<frame_contracts::Quality>),
    Codec(frame_contracts::RecordingCodec),
    Encoder(frame_contracts::EncoderPreference),
    Profile(frame_contracts::RecordingPreset),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum VideoAction {
    Audio(crate::ui::components::AudioSettingsEvent),
    Performance(frame_contracts::PerformancePolicy),
    CameraDevice(String),
    CameraFormat(u32),
    CameraToggle,
    Mode(VideoMode),
    #[allow(dead_code)]
    Expand(VideoSection),
    Edit(VideoEdit),
    Refresh,
    FpsEditor,
    #[allow(dead_code)]
    ResolutionEditor,
    BitrateEditor,
    QpEditor,
    GopEditor,
}

#[derive(Default)]
pub(crate) struct VideoUiState {
    pub mode: VideoMode,
    pub expanded: [bool; 7],
    pub capabilities: Option<VideoCapabilities>,
    pub probing: Option<VideoContext>,
    pub probe_generation: u64,
    pub probe_error: Option<String>,
}
impl VideoUiState {
    pub fn switch_mode(&mut self, mode: VideoMode) {
        self.mode = mode;
    }
    pub fn capabilities_for(&self, context: &VideoContext) -> Option<&VideoCapabilities> {
        self.capabilities.as_ref().filter(|c| &c.context == context)
    }
    pub fn begin_probe(&mut self, context: VideoContext) -> u64 {
        self.probe_generation += 1;
        self.probing = Some(context);
        self.probe_error = None;
        self.probe_generation
    }
    pub fn finish_probe(&mut self, generation: u64, result: Result<VideoCapabilities, String>) {
        if generation != self.probe_generation {
            return;
        }
        self.probing = None;
        match result {
            Ok(caps) => {
                self.capabilities = Some(caps);
                self.probe_error = None;
            }
            Err(error) => {
                self.capabilities = None;
                self.probe_error = Some(error);
            }
        }
    }
}
