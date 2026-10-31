//! Pure application presentation state. Media readiness remains authoritative.
use frame_contracts::*;
use frame_runtime::recording_control::{AUTO_FILE_STEM, PublishedState};
use serde::{Deserialize, Serialize};
pub mod workspace;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceMode {
    Screen,
    Window,
    Area,
    Camera,
}
impl SourceMode {
    pub const ALL: [Self; 4] = [Self::Screen, Self::Window, Self::Area, Self::Camera];
    pub fn label(self) -> &'static str {
        match self {
            Self::Screen => "Screen",
            Self::Window => "Window",
            Self::Area => "Area",
            Self::Camera => "Camera",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sheet {
    Sources,
    Settings,
    Quality,
    Options,
    Audio,
    Camera,
    Recovery,
    Diagnostics,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecorderSection {
    Audio,
    Video,
    Camera,
    Quality,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecorderSections {
    pub audio: bool,
    pub video: bool,
    pub camera: bool,
    pub quality: bool,
}
impl RecorderSections {
    pub fn toggle(&mut self, section: RecorderSection) {
        if section == RecorderSection::Audio {
            self.audio = !self.audio;
            return;
        }
        let was_open = match section {
            RecorderSection::Video => self.video,
            RecorderSection::Camera => self.camera,
            RecorderSection::Quality => self.quality,
            RecorderSection::Audio => unreachable!(),
        };
        self.video = section == RecorderSection::Video && !was_open;
        self.camera = section == RecorderSection::Camera && !was_open;
        self.quality = section == RecorderSection::Quality && !was_open;
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    #[serde(default)]
    pub appearance_revision: u32,
    pub theme: crate::ui::theme::ThemeId,
    pub surface: crate::ui::theme::SurfacePreference,
    pub frosted_transparency: u8,
    pub accent_color: Option<u32>,
    pub recorder_sections: RecorderSections,
    pub workspace: workspace::DockLayout,
    pub low_cost_effects: bool,
    pub filled_buttons: bool,
    pub automatic_updates: bool,
    // Retain fields written by a newer application if an update rolls back.
    #[serde(flatten)]
    pub extra_preferences: std::collections::BTreeMap<String, serde_json::Value>,
    pub artwork: bool,
    pub reduced_motion: bool,
    pub countdown: u8,
    pub interaction_metadata: bool,
    pub disabled_camera: Option<CameraSelection>,
    pub camera_format: Option<CameraFormat>,
    pub performance: PerformancePolicy,
    pub replay_settings: ReplaySettings,
    pub last_recording: Option<String>,
    pub last_replay: Option<String>,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            appearance_revision: 2,
            theme: Default::default(),
            surface: Default::default(),
            frosted_transparency: 20,
            accent_color: None,
            recorder_sections: Default::default(),
            workspace: Default::default(),
            low_cost_effects: false,
            filled_buttons: false,
            automatic_updates: true,
            extra_preferences: Default::default(),
            artwork: false,
            reduced_motion: false,
            countdown: 0,
            interaction_metadata: false,
            disabled_camera: None,
            camera_format: None,
            performance: Default::default(),
            replay_settings: ReplaySettings::default(),
            last_recording: None,
            last_replay: None,
        }
    }
}

impl Preferences {
    /// Adopt the new clean default once, without resetting capture/audio settings.
    pub(crate) fn migrate_appearance(&mut self) -> bool {
        if self.appearance_revision >= 2 {
            return false;
        }
        if self.appearance_revision == 0
            && self.theme == crate::ui::theme::ThemeId::White
            && self.artwork
        {
            self.theme = crate::ui::theme::ThemeId::Black;
        }
        if self.appearance_revision == 0 {
            self.artwork = false;
        }
        self.appearance_revision = 2;
        true
    }
}

pub struct AppModel {
    pub snapshot: EngineSnapshot,
    pub request: Option<RecordingRequest>,
    pub preferences: Preferences,
    pub mode: SourceMode,
    pub sheet: Option<Sheet>,
    pub connecting: bool,
    pub command_pending: bool,
    pub replay_save_pending: bool,
    pub pending_route: Option<(AudioSourceId, AudioRoute)>,
    pub settings_blocked: bool,
    pub shortcuts: ShortcutBindings,
    pub shortcuts_pending: bool,
    pub closing: bool,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub interrupted: Vec<String>,
    pub recovery_selected: Option<String>,
    pub recovery_metadata: std::collections::BTreeMap<String, String>,
    pub recovery_busy: bool,
    pub recovery_result: Option<RecordingRecoveryResult>,
    pub level_testing: bool,
    pub cameras: Vec<CameraInfo>,
    pub formats: Vec<CameraFormat>,
    pub countdown_deadline: Option<std::time::Instant>,
    pub preparation_timed_out: bool,
    pub staged_target: Option<CaptureTarget>,
}
impl Default for AppModel {
    fn default() -> Self {
        Self {
            snapshot: frame_core_snapshot(),
            request: None,
            preferences: Preferences::default(),
            mode: SourceMode::Screen,
            sheet: None,
            connecting: true,
            command_pending: false,
            replay_save_pending: false,
            pending_route: None,
            settings_blocked: false,
            shortcuts: ShortcutBindings::default(),
            shortcuts_pending: false,
            closing: false,
            error: None,
            notice: None,
            interrupted: vec![],
            recovery_selected: None,
            recovery_metadata: Default::default(),
            recovery_busy: false,
            recovery_result: None,
            level_testing: false,
            cameras: vec![],
            formats: vec![],
            countdown_deadline: None,
            preparation_timed_out: false,
            staged_target: None,
        }
    }
}
fn frame_core_snapshot() -> EngineSnapshot {
    EngineSnapshot {
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
impl AppModel {
    pub fn required_quality(&self) -> Option<Quality> {
        let id = match &self.request.as_ref()?.target {
            CaptureTarget::Display(id) => id,
            CaptureTarget::Region(region) => &region.display_id,
            _ => return None,
        };
        self.snapshot
            .displays
            .iter()
            .find(|d| &d.id == id)?
            .required_recording_quality
    }
    pub fn automatic_quality(&self) -> bool {
        self.request.as_ref().is_none_or(|r| {
            r.optimization
                .as_ref()
                .is_some_and(|o| o.automatic_quality && o.preset != RecordingPreset::Custom)
        })
    }
    pub fn set_automatic_quality(&mut self) {
        let quality = self.required_quality().unwrap_or(Quality::Standard);
        if let Some(request) = &mut self.request {
            request.quality = quality;
            let options = request
                .optimization
                .get_or_insert_with(RecordingOptimization::default);
            options.automatic_quality = true;
            if options.preset == RecordingPreset::Custom {
                options.preset = RecordingPreset::Desktop;
            }
        }
    }
    pub fn set_preset(&mut self, preset: RecordingPreset) {
        if let Some(request) = &mut self.request {
            let options = request
                .optimization
                .get_or_insert_with(|| RecordingOptimization {
                    automatic_quality: false,
                    ..RecordingOptimization::default()
                });
            options.preset = preset;
            if preset == RecordingPreset::Custom {
                options.automatic_quality = false;
            }
        }
    }
    pub fn set_quality(&mut self, quality: Quality) -> bool {
        if self.required_quality().is_some_and(|q| q != quality) {
            return false;
        }
        if let Some(request) = &mut self.request {
            request.quality = quality;
            request
                .optimization
                .get_or_insert_with(RecordingOptimization::default)
                .automatic_quality = false;
            true
        } else {
            false
        }
    }
    pub fn editable(&self) -> bool {
        !self.connecting
            && !self.command_pending
            && !self.closing
            && !self.recovery_busy
            && self.snapshot.state == RecorderState::Idle
    }
    pub fn initialize(
        &mut self,
        mut snapshot: EngineSnapshot,
        published: PublishedState,
        saved: Option<RecordingRequest>,
        output: String,
    ) {
        snapshot.audio_sources = published.audio_sources.clone();
        self.snapshot = snapshot;
        self.apply_published(published);
        self.request = saved.or_else(|| {
            self.snapshot
                .displays
                .iter()
                .find(|d| d.is_primary)
                .or_else(|| self.snapshot.displays.first())
                .map(|d| RecordingRequest {
                    target: CaptureTarget::Display(d.id.clone()),
                    fps_mode: FpsMode::AutoMatchSource,
                    resolution: Resolution::Native,
                    quality: Quality::Standard,
                    encoder: EncoderPreference::Auto,
                    optimization: Some(RecordingOptimization::default()),
                    audio_encoding: Default::default(),
                    audio_routes: vec![],
                    camera: None,

                    show_cursor: true,
                    output: OutputSettings {
                        directory: output.clone(),
                        file_stem: AUTO_FILE_STEM.into(),
                    },
                })
        });
        self.reconcile_sources();
        if let Some(r) = self.request.as_mut() {
            if r.output.directory.is_empty() || r.output.directory == "." {
                r.output.directory = output;
            }
            // Retain the persisted recording filename/template. Legacy automatic
            // naming remains the default for requests without a saved selection.
            self.mode = match r.target {
                CaptureTarget::Display(_) => SourceMode::Screen,
                CaptureTarget::Window(_) | CaptureTarget::Game(_) => SourceMode::Window,
                CaptureTarget::Region(_) => SourceMode::Area,
            };
        }
        self.connecting = false;
    }
    /// Reconcile discovery only while idle; keep valid composition and all other preferences.
    pub fn reconcile_sources(&mut self) -> bool {
        if !matches!(
            self.snapshot.state,
            RecorderState::Idle | RecorderState::Failed(_)
        ) {
            return false;
        }
        let Some(request) = &mut self.request else {
            return false;
        };
        let mut changed = false;
        let valid = match &request.target {
            CaptureTarget::Display(id) => self.snapshot.displays.iter().any(|d| &d.id == id),
            CaptureTarget::Region(r) => self.snapshot.displays.iter().any(|d| d.id == r.display_id),
            CaptureTarget::Window(id) => self.snapshot.windows.iter().any(|w| &w.id == id),
            _ => true,
        };
        if !valid
            && let Some(display) = self
                .snapshot
                .displays
                .iter()
                .find(|d| d.is_primary)
                .or_else(|| self.snapshot.displays.first())
        {
            request.target = CaptureTarget::Display(display.id.clone());
            request.resolution = Resolution::Native;
            if let Some(quality) = display.required_recording_quality {
                request.quality = quality;
            }
            self.mode = SourceMode::Screen;
            changed = true;
        }
        changed
    }
    pub fn apply_published(&mut self, p: PublishedState) {
        self.countdown_deadline = p.countdown_deadline;
        self.preparation_timed_out = p.preparation_timed_out;
        self.snapshot.sequence = p.sequence;
        self.snapshot.session_id = p.session_id;
        self.snapshot.state = p.state;
        self.snapshot.metrics = p.metrics;
        self.snapshot.last_artifact = p.last_artifact;
        self.snapshot.replay_settings = p.replay_settings;
        self.snapshot.replay_active = p.replay_active;
        self.snapshot.replay_only = p.replay_only;
        self.snapshot.last_replay = p.last_replay;
        self.snapshot.plan_decisions = p.plan_decisions;
        self.snapshot.audio_sources = p.audio_sources;
        if self.pending_route.as_ref().is_some_and(|(id, route)| {
            self.snapshot
                .audio_sources
                .iter()
                .any(|s| &s.id == id && &s.route == route)
        }) {
            self.pending_route = None;
            self.command_pending = false;
        }
    }
    pub fn source_label(&self) -> String {
        let Some(r) = &self.request else {
            return "Discovering your sourcesâ€¦".into();
        };
        match &r.target {
            CaptureTarget::Display(id) => self
                .snapshot
                .displays
                .iter()
                .find(|d| &d.id == id)
                .map(|d| format!("{} Â· {} Ã— {}", d.name, d.width, d.height)),
            CaptureTarget::Region(r) => {
                Some(format!("Selected area Â· {} Ã— {}", r.width, r.height))
            }
            CaptureTarget::Window(id) => self
                .snapshot
                .windows
                .iter()
                .find(|w| &w.id == id)
                .map(|w| w.title.clone()),
            CaptureTarget::Game(g) => Some(format!("Windowed game Â· {}", g.process_id)),
        }
        .unwrap_or_else(|| "Choose an available source".into())
    }
    pub fn can_start(&self) -> bool {
        self.editable()
            && !self.settings_blocked
            && self.snapshot.capabilities.can_record
            && self.request.as_ref().is_some_and(|r| {
                frame_settings::validate_request(&r.for_capture()).is_ok()
                    && match r.resolution {
                        Resolution::Native => true,
                        Resolution::Exact(size) => {
                            self.source_dimensions() == Some((size.width, size.height))
                        }
                    }
                    && (r.fps_mode == FpsMode::AutoMatchSource
                        || self.snapshot.capabilities.fixed_fps)
                    && match r.fps_mode {
                        FpsMode::AutoMatchSource => true,
                        FpsMode::Fixed(rate) => rate.is_finite() && rate > 0.0,
                    }
                    && r.camera.as_ref().is_none_or(|camera| {
                        self.snapshot.capabilities.camera_overlay
                            && self.source_dimensions().is_some_and(|(w, h)| {
                                camera.placement.width > 0
                                    && camera.placement.height > 0
                                    && camera
                                        .placement
                                        .x
                                        .checked_add(camera.placement.width)
                                        .is_some_and(|x| x <= w)
                                    && camera
                                        .placement
                                        .y
                                        .checked_add(camera.placement.height)
                                        .is_some_and(|y| y <= h)
                            })
                    })
                    && !r.output.directory.trim().is_empty()
                    && match &r.target {
                        CaptureTarget::Display(id) => self.snapshot.displays.iter().any(|d| {
                            &d.id == id
                                && d.required_recording_quality.is_none_or(|q| q == r.quality)
                        }),
                        CaptureTarget::Region(region) => {
                            self.snapshot.capabilities.region_capture
                                && self
                                    .snapshot
                                    .displays
                                    .iter()
                                    .find(|d| d.id == region.display_id)
                                    .is_some_and(|d| {
                                        d.required_recording_quality.is_none_or(|q| q == r.quality)
                                            && frame_settings::validate_region(region, d).is_ok()
                                    })
                        }
                        CaptureTarget::Window(id) => {
                            self.snapshot.capabilities.window_capture
                                && r.camera.is_none()
                                && self.snapshot.windows.iter().any(|w| &w.id == id)
                        }
                        CaptureTarget::Game(_) => self.snapshot.capabilities.game_capture,
                    }
            })
    }
    pub fn source_rate_hz(&self) -> Option<f64> {
        let id = match &self.request.as_ref()?.target {
            CaptureTarget::Display(id) => id,
            CaptureTarget::Region(region) => &region.display_id,
            _ => return None,
        };
        self.snapshot
            .displays
            .iter()
            .find(|d| &d.id == id)?
            .refresh_rate_hz
    }

    pub fn source_dimensions(&self) -> Option<(u32, u32)> {
        match &self.request.as_ref()?.target {
            CaptureTarget::Display(id) => self
                .snapshot
                .displays
                .iter()
                .find(|d| &d.id == id)
                .map(|d| (d.width, d.height)),
            CaptureTarget::Region(region) => Some((region.width, region.height)),
            _ => None,
        }
    }
    pub fn selected_routes(&self) -> Vec<AudioRouteSelection> {
        self.snapshot
            .audio_sources
            .iter()
            .map(|s| AudioRouteSelection {
                controls: s.controls,
                muted: s.muted,
                source_id: s.id.clone(),
                route: s.route,
            })
            .collect()
    }
    pub fn sync_routes(&mut self) {
        let routes = self.selected_routes();
        if let Some(r) = &mut self.request {
            r.audio_routes = routes;
        }
    }
    pub fn record_label(&self) -> &'static str {
        if self.closing {
            return "Closing safelyâ€¦";
        }
        if self.command_pending {
            return "Please waitâ€¦";
        }
        if self.snapshot.replay_only {
            return if self.snapshot.state == RecorderState::Finalizing {
                "Stopping replayâ€¦"
            } else {
                "Stop replay buffer"
            };
        }
        match self.snapshot.state {
            RecorderState::Idle => "Start recording",
            RecorderState::Preparing => "Cancel preparation",
            RecorderState::Recording => "Stop recording",
            RecorderState::Paused => "Finish recording",
            RecorderState::Finalizing => "Finalizing recordingâ€¦",
            RecorderState::Failed(_) => "Reset recorder",
        }
    }
    pub fn replay_wait_seconds(&self) -> u32 {
        let elapsed = self
            .snapshot
            .metrics
            .as_ref()
            .map_or(0., |m| m.elapsed_ms.max(0.));
        (f64::from(self.snapshot.replay_settings.duration_seconds) - elapsed / 1000.)
            .max(0.)
            .ceil() as u32
    }
    pub fn can_save_replay(&self) -> bool {
        self.snapshot.replay_active
            && !self.command_pending
            && !self.replay_save_pending
            && !self.closing
            && matches!(
                self.snapshot.state,
                RecorderState::Recording | RecorderState::Paused
            )
            && self.replay_wait_seconds() == 0
    }
    pub fn replay_save_label(&self) -> String {
        if self.replay_save_pending {
            "Saving replayâ€¦".into()
        } else if !self.snapshot.replay_active {
            "Save replay".into()
        } else if self.replay_wait_seconds() > 0 {
            format!("Replay ready in {}s", self.replay_wait_seconds())
        } else {
            "Save replay".into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn model() -> AppModel {
        let mut model = AppModel::default();
        let mut snapshot = model.snapshot.clone();
        snapshot.capabilities.can_record = true;
        snapshot.capabilities.region_capture = true;
        snapshot.capabilities.fixed_fps = true;
        snapshot.displays.push(DisplayInfo {
            id: DisplayId("screen".into()),
            name: "Screen".into(),
            width: 2560,
            height: 1600,
            x: 0,
            y: 0,
            scale_factor: 1.25,
            refresh_rate_hz: Some(165.),
            is_primary: true,
            hdr_available: false,
            required_recording_quality: None,
        });
        model.initialize(
            snapshot,
            PublishedState::default(),
            None,
            "recordings".into(),
        );
        model
    }
    #[test]
    fn automatic_quality_respects_mandatory_source_profile_and_preserves_routes_output() {
        let mut model = model();
        let output = model.request.as_ref().unwrap().output.clone();
        assert!(model.automatic_quality());
        model.request.as_mut().unwrap().fps_mode = FpsMode::Fixed(60.);
        assert!(model.set_quality(Quality::Maximum));
        assert!(!model.automatic_quality());
        model.snapshot.displays[0].required_recording_quality = Some(Quality::High);
        model.set_automatic_quality();
        assert!(model.automatic_quality());
        assert!(model.can_start());
        assert_eq!(model.request.as_ref().unwrap().quality, Quality::High);
        assert_eq!(model.request.as_ref().unwrap().output, output);
        assert_eq!(
            model.request.as_ref().unwrap().fps_mode,
            FpsMode::Fixed(60.)
        );
        model.set_preset(RecordingPreset::Gaming);
        assert_eq!(
            model.request.as_ref().unwrap().fps_mode,
            FpsMode::Fixed(60.)
        );
        model.request.as_mut().unwrap().fps_mode = FpsMode::Fixed(480.);
        assert!(model.can_start());
        assert!(!model.set_quality(Quality::Standard));
        assert_eq!(model.request.as_ref().unwrap().quality, Quality::High);
    }
    #[test]
    fn published_transitions_control_editability_and_record_action() {
        let mut model = model();
        for (sequence, state, editable, label) in [
            (1, RecorderState::Preparing, false, "Cancel preparation"),
            (2, RecorderState::Recording, false, "Stop recording"),
            (3, RecorderState::Paused, false, "Finish recording"),
            (
                4,
                RecorderState::Finalizing,
                false,
                "Finalizing recordingâ€¦",
            ),
            (5, RecorderState::Idle, true, "Start recording"),
        ] {
            model.apply_published(PublishedState {
                sequence,
                state,
                ..Default::default()
            });
            assert_eq!(model.snapshot.sequence, sequence);
            assert_eq!(model.editable(), editable);
            assert_eq!(model.record_label(), label);
        }
    }
    #[test]
    fn admission_checks_quality_region_and_saved_unsupported_options() {
        let mut model = model();
        assert!(model.can_start());
        model.snapshot.displays[0].required_recording_quality = Some(Quality::High);
        assert!(!model.can_start());
        model.request.as_mut().unwrap().quality = Quality::High;
        assert!(model.can_start());
        model.request.as_mut().unwrap().target = CaptureTarget::Region(Region {
            display_id: DisplayId("screen".into()),
            x: 2500,
            y: 0,
            width: 200,
            height: 200,
        });
        assert!(!model.can_start());
        model.request.as_mut().unwrap().target = CaptureTarget::Region(Region {
            display_id: DisplayId("screen".into()),
            x: 0,
            y: 0,
            width: 200,
            height: 200,
        });
        assert!(model.can_start());
        model.request.as_mut().unwrap().fps_mode = FpsMode::Fixed(f64::NAN);
        assert!(!model.can_start());
        model.request.as_mut().unwrap().fps_mode = FpsMode::AutoMatchSource;
        model.request.as_mut().unwrap().resolution = Resolution::Exact(Dimensions {
            width: 1280,
            height: 720,
        });
        assert!(!model.can_start());
    }
    #[test]
    fn route_admission_does_not_claim_worker_completion() {
        let mut model = model();
        let id = AudioSourceId("mic".into());
        let source = AudioSource {
            controls: Default::default(),
            capture_identity: None,
            muted: false,
            id: id.clone(),
            kind: AudioSourceKind::Microphone,
            display_name: "Mic".into(),
            process_id: None,
            app_identifier: None,
            is_active: true,
            route: AudioRoute::Ignore,
        };
        model.pending_route = Some((id, AudioRoute::MainMix));
        model.command_pending = true;
        model.apply_published(PublishedState {
            audio_sources: vec![source.clone()],
            ..Default::default()
        });
        assert!(model.command_pending);
        assert!(!model.can_start());
        model.apply_published(PublishedState {
            sequence: 1,
            audio_sources: vec![AudioSource {
                controls: Default::default(),
                capture_identity: None,
                route: AudioRoute::MainMix,
                ..source
            }],
            ..Default::default()
        });
        model.sync_routes();
        assert!(!model.command_pending);
        assert!(model.can_start());
        assert_eq!(
            model.request.unwrap().audio_routes[0].route,
            AudioRoute::MainMix
        );
    }
    #[test]
    fn readiness_and_busy_gates_are_authoritative() {
        let mut m = AppModel {
            connecting: false,
            ..Default::default()
        };
        m.snapshot.state = RecorderState::Preparing;
        assert!(!m.editable());
        assert_eq!(m.record_label(), "Cancel preparation");
        m.snapshot.state = RecorderState::Recording;
        assert_eq!(m.record_label(), "Stop recording");
        m.snapshot.state = RecorderState::Paused;
        assert_eq!(m.record_label(), "Finish recording");
        m.snapshot.state = RecorderState::Finalizing;
        assert!(!m.can_start());
        m.snapshot.state = RecorderState::Idle;
        m.settings_blocked = true;
        assert!(!m.can_start());
        m.closing = true;
        assert!(!m.editable());
    }
}
