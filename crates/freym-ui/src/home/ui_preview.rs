//! Opt-in local workbench using the production Home render and synthetic state.
use super::*;
use crate::ui::{
    primitives::{Button, ButtonSize, FocusNext, FocusPrevious, Stack, Surface, Text, TextVariant},
    theme::{colors::Color, spacing::Spacing},
};
use gpui::{AppContext, Render, div};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scene {
    Home,
    Components,
    Settings,
    Sources,
    Audio,
    Camera,
    Countdown,
    Recording,
    Paused,
    Stopping,
    Error,
    Recovery,
}
impl Scene {
    const ALL: [Self; 12] = [
        Self::Home,
        Self::Components,
        Self::Settings,
        Self::Sources,
        Self::Audio,
        Self::Camera,
        Self::Countdown,
        Self::Recording,
        Self::Paused,
        Self::Stopping,
        Self::Error,
        Self::Recovery,
    ];
    fn label(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Components => "Components",
            Self::Settings => "Settings",
            Self::Sources => "Sources",
            Self::Audio => "Audio",
            Self::Camera => "Camera",
            Self::Countdown => "Countdown",
            Self::Recording => "Recording",
            Self::Paused => "Paused",
            Self::Stopping => "Stopping",
            Self::Error => "Error",
            Self::Recovery => "Recovery",
        }
    }
    fn id(self) -> &'static str {
        match self {
            Self::Home => "preview-Home",
            Self::Components => "preview-Components",
            Self::Settings => "preview-Settings",
            Self::Sources => "preview-Sources",
            Self::Audio => "preview-Audio",
            Self::Camera => "preview-Camera",
            Self::Countdown => "preview-Countdown",
            Self::Recording => "preview-Recording",
            Self::Paused => "preview-Paused",
            Self::Stopping => "preview-Stopping",
            Self::Error => "preview-Error",
            Self::Recovery => "preview-Recovery",
        }
    }
}

pub struct PreviewWorkbench {
    home: Entity<Home>,
    components: Entity<crate::ui::component_lab::ComponentLab>,
    scene: Scene,
}
impl PreviewWorkbench {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let home = cx.new(|cx| Home::new_ui_preview(window, cx));
        cx.observe(&home, |_, _, cx| cx.notify()).detach();
        let mut workbench = Self {
            home,
            components: cx.new(crate::ui::component_lab::ComponentLab::new),
            scene: Scene::Home,
        };
        if std::env::args().any(|arg| arg == "--components") {
            workbench.scene = Scene::Components;
        }
        workbench
    }
}
impl Render for PreviewWorkbench {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.scene != Scene::Components {
            let preferences = &self.home.read(cx).model.preferences;
            preferences.theme.activate();
            crate::ui::theme::set_filled_buttons(preferences.filled_buttons);
            crate::ui::theme::set_low_cost_effects(preferences.low_cost_effects);
        }
        let scenes = div()
            .flex()
            .flex_wrap()
            .gap(Spacing::Xxs.px())
            .children(Scene::ALL.map(|scene| {
                Button::new(scene.id(), scene.label())
                    .size(ButtonSize::Compact)
                    .selected(self.scene == scene)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.scene = scene;
                        this.home
                            .update(cx, |home, cx| home.set_preview_scene(scene, window, cx));
                        cx.notify();
                    }))
            }));
        div()
            .id("ui-preview-workbench")
            .size_full()
            .flex()
            .flex_col()
            .bg(Color::Background.get())
            .key_context("FREYM")
            .on_action(|_: &FocusNext, w, cx| w.focus_next(cx))
            .on_action(|_: &FocusPrevious, w, cx| w.focus_prev(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(if self.scene == Scene::Components {
                        self.components.clone().into_any_element()
                    } else {
                        self.home.clone().into_any_element()
                    }),
            )
            .child(
                div().flex_shrink_0().child(
                    Surface::new().child(
                        Stack::vertical()
                            .gap(Spacing::Xxs)
                            .child(
                                Text::new(
                                    "GPUI UI LAB · Example data · Components uses production code · Save Rust files to rebuild",
                                )
                                .variant(TextVariant::Caption),
                            )
                            .child(scenes),
                    ),
                ),
            )
    }
}

impl Home {
    fn new_ui_preview(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // No discovery, settings load, recovery scan or global hotkey registration.
        // Every action with an external effect is intercepted below.
        let (services, _) = Services::new();
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let home = Self {
            video: Default::default(),
            ui_preview: true,
            model: example_model(Scene::Home),
            services,
            focus,
            mic: cx.new(|_| AudioMeter::default()),
            system: cx.new(|_| AudioMeter::default()),
            meter_task: None,
            save_running: false,
            save_latest: None,
            settings_document: None,
            settings_committed: None,
            settings_category: 0,
            render_count: 0,
            hotkeys: None,
            shortcut_edit: None,
            number_editor: None,
            #[cfg(target_os = "windows")]
            preview: None,
            preview_enabled: false,
            studios: Default::default(),
            studio_prompt_pending: false,
            owner_window: window.window_handle(),
            start_intent: None,
            compact: None,
            compact_position: None,
            pending_mute: None,
            audio_ui: Default::default(),
            updates: None,
            update_snapshot: Default::default(),
            update_running: false,
            update_monitor: None,
            preferences_loaded: true,
        };
        let view = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            let _ = view.update(cx, |home, cx| home.close(window, cx));
            false
        });
        cx.observe_window_bounds(window, |_, _, cx| cx.notify())
            .detach();
        home
    }
    fn set_preview_scene(&mut self, scene: Scene, window: &mut Window, cx: &mut Context<Self>) {
        let prefs = self.model.preferences.clone();
        self.model = example_model(scene);
        self.model.preferences = prefs;
        self.number_editor = None;
        self.shortcut_edit = None;
        self.mic.update(cx, |m, cx| m.sample(None, None, cx));
        self.system.update(cx, |m, cx| m.sample(None, None, cx));
        window.focus(&self.focus, cx);
        cx.notify();
    }
    /// Exhaustive classification keeps new product actions from accidentally
    /// reaching a real backend in the preview. Pure edits reuse production code.
    pub(super) fn dispatch_preview(
        &mut self,
        action: &Action,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match action {
            Action::Video(_) => return false,
            Action::SettingsCategory(_) => return false,
            Action::UseSupportedAudio => return false,
            Action::RecordingFilename => return false,
            Action::RollbackSettings | Action::VerifyRecording => {
                self.model.notice =
                    Some("UI preview: no saved settings or recordings are modified.".into());
            }
            Action::AudioSettings(event) => {
                use crate::ui::components::AudioSettingsEvent;
                match event {
                    AudioSettingsEvent::Mode(mode) => {
                        if self.settings_document.is_none() {
                            self.install_settings_document(None);
                        }
                        if let Some(d) = &mut self.settings_document {
                            d.modes.audio =
                                if *mode == crate::ui::components::AudioSettingsMode::Advanced {
                                    frame_settings::model::SettingsMode::Advanced
                                } else {
                                    frame_settings::model::SettingsMode::Easy
                                };
                        }
                    }
                    AudioSettingsEvent::Route(id, route) => self.preview_route(id, *route),
                    AudioSettingsEvent::Controls(id, controls) => {
                        if controls.validate().is_ok() {
                            if let Some(s) = self
                                .model
                                .snapshot
                                .audio_sources
                                .iter_mut()
                                .find(|s| &s.id == id)
                            {
                                s.controls = *controls;
                            }
                            self.model.sync_routes();
                        }
                    }
                    AudioSettingsEvent::Mute(id, muted) => {
                        if let Some(s) = self
                            .model
                            .snapshot
                            .audio_sources
                            .iter_mut()
                            .find(|s| &s.id == id)
                        {
                            s.muted = *muted;
                        }
                        self.model.sync_routes();
                    }
                    _ => {
                        self.model.notice =
                            Some("UI preview: device commands are unavailable.".into());
                    }
                }
            }
            Action::ToggleSection(_) => return false,
            Action::FindCameras => {
                self.model.notice = Some("UI preview: example cameras are already listed.".into());
            }
            Action::Compact => return false,
            Action::AutomaticQuality
            | Action::SelectQuality(_)
            | Action::SelectCameraFormat(_)
            | Action::RemoveCamera
            | Action::Shortcut(_)
            | Action::Fps
            | Action::CameraPlacement
            | Action::ReplayMemory
            | Action::ApplyNumber
            | Action::AutoFps
            | Action::CancelNumber
            | Action::Diagnostics
            | Action::Settings
            | Action::QualityOptions
            | Action::MoreOptions
            | Action::Audio
            | Action::Theme(_)
            | Action::LowCostEffects
            | Action::FilledButtons
            | Action::AutomaticUpdates
            | Action::Motion
            | Action::Cursor
            | Action::Countdown
            | Action::CloseSheet => return false,
            Action::CheckUpdates => {}
            Action::Source(mode) => {
                if self.model.editable() {
                    self.model.mode = *mode;
                    self.model.sheet = Some(if *mode == SourceMode::Camera {
                        Sheet::Camera
                    } else {
                        Sheet::Sources
                    });
                }
            }
            Action::OpenSourcePicker => {
                self.model.mode = match self.model.request.as_ref().map(|r| &r.target) {
                    Some(CaptureTarget::Window(_)) => SourceMode::Window,
                    Some(CaptureTarget::Region(_)) => SourceMode::Area,
                    _ => SourceMode::Screen,
                };
                self.model.sheet = Some(Sheet::Sources);
            }
            Action::SelectDisplay(id) => {
                if self.model.editable()
                    && let Some(d) = self.model.snapshot.displays.iter().find(|d| &d.id == id)
                    && let Some(r) = &mut self.model.request
                {
                    r.target = if self.model.mode == SourceMode::Area {
                        CaptureTarget::Region(Region {
                            display_id: id.clone(),
                            x: 100,
                            y: 100,
                            width: 1280,
                            height: 720,
                        })
                    } else {
                        CaptureTarget::Display(id.clone())
                    };
                    r.quality = d.required_recording_quality.unwrap_or(Quality::Standard);
                    self.model.sheet = None;
                }
            }
            Action::SelectWindow(id) => {
                if self.model.editable()
                    && let Some(r) = &mut self.model.request
                {
                    r.target = CaptureTarget::Window(id.clone());
                    self.model.mode = SourceMode::Window;
                    self.model.sheet = None;
                }
            }
            Action::SelectRoute(id, route) => self.preview_route(id, *route),
            Action::ToggleAudio(id) => {
                if let Some(source) = self
                    .model
                    .snapshot
                    .audio_sources
                    .iter()
                    .find(|s| &s.id == id)
                {
                    let route = if source.route == AudioRoute::Ignore {
                        AudioRoute::MainMix
                    } else {
                        AudioRoute::Ignore
                    };
                    self.preview_route(id, route);
                }
            }
            Action::MuteAudio(id, muted) => {
                if self.model.editable()
                    || matches!(
                        self.model.snapshot.state,
                        RecorderState::Recording | RecorderState::Paused
                    )
                {
                    if let Some(source) = self
                        .model
                        .snapshot
                        .audio_sources
                        .iter_mut()
                        .find(|s| &s.id == id)
                    {
                        source.muted = *muted;
                    }
                    if let Some(request) = &mut self.model.request {
                        for route in &mut request.audio_routes {
                            if &route.source_id == id {
                                route.muted = *muted;
                            }
                        }
                    }
                }
            }
            Action::SelectCamera(id) => {
                if self.model.editable()
                    && let Some(r) = &mut self.model.request
                {
                    r.camera = Some(example_camera(id.clone()));
                }
            }
            Action::Record => {
                self.model.snapshot.state = match self.model.snapshot.state {
                    RecorderState::Idle => RecorderState::Recording,
                    RecorderState::Recording | RecorderState::Paused => RecorderState::Finalizing,
                    RecorderState::Preparing | RecorderState::Failed(_) => RecorderState::Idle,
                    RecorderState::Finalizing => return true,
                };
                self.model.snapshot.metrics = Some(example_metrics());
                self.model.error = None;
            }
            Action::Pause => {
                self.model.snapshot.state = match self.model.snapshot.state {
                    RecorderState::Recording => RecorderState::Paused,
                    RecorderState::Paused => RecorderState::Recording,
                    _ => return true,
                };
            }
            Action::TestLevels => {
                self.model.level_testing = !self.model.level_testing;
                let level = self.model.level_testing.then_some(0.15);
                self.mic.update(cx, |m, cx| m.sample(level, level, cx));
                self.system.update(cx, |m, cx| m.sample(level, level, cx));
            }
            Action::Recovery => self.model.sheet = Some(Sheet::Recovery),
            Action::RecoverFile(_) => {
                self.model.notice =
                    Some("UI preview: recovery action selected. No file was opened.".into())
            }
            Action::Replay => {
                self.model.snapshot.replay_settings.duration_seconds =
                    match self.model.snapshot.replay_settings.duration_seconds {
                        15 => 30,
                        30 => 60,
                        _ => 15,
                    };
            }
            Action::Output => {
                self.model.notice = Some("UI preview: output folder action selected.".into())
            }
            Action::Preview
            | Action::PickWindow
            | Action::ToggleReplay
            | Action::Studio
            | Action::StudioRecording
            | Action::StudioReplay
            | Action::InteractionMetadata
            | Action::SaveReplay
            | Action::RevealRecording => {
                self.model.notice = Some("UI preview: this action is simulated.".into());
            }
            Action::Refresh => {
                self.model.notice = Some("UI preview: example sources are ready.".into())
            }
            Action::RetrySettings => {
                self.model.settings_blocked = false;
                self.model.error = None;
            }
        }
        true
    }
    fn preview_route(&mut self, id: &AudioSourceId, route: AudioRoute) {
        if !self.model.editable() {
            return;
        }
        if let Some(s) = self
            .model
            .snapshot
            .audio_sources
            .iter_mut()
            .find(|s| &s.id == id)
        {
            s.route = route;
        }
        if let Some(r) = &mut self.model.request {
            r.audio_routes = self
                .model
                .snapshot
                .audio_sources
                .iter()
                .map(|s| AudioRouteSelection {
                    controls: s.controls,
                    muted: s.muted,
                    source_id: s.id.clone(),
                    route: s.route,
                })
                .collect();
        }
    }
}

fn example_model(scene: Scene) -> AppModel {
    let mut model = AppModel {
        connecting: false,
        ..Default::default()
    };
    model.snapshot.displays = [
        ("preview-display-1", "Display 1", 2560, 1440, 165.),
        ("preview-display-2", "Display 2", 1920, 1080, 60.),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (id, name, width, height, hz))| DisplayInfo {
        id: DisplayId(id.into()),
        name: name.into(),
        width,
        height,
        x: if index == 0 { 0 } else { 2560 },
        y: 0,
        scale_factor: 1.,
        refresh_rate_hz: Some(hz),
        is_primary: index == 0,
        hdr_available: false,
        required_recording_quality: None,
    })
    .collect();
    model.snapshot.windows = ["Design review", "Code editor"]
        .into_iter()
        .enumerate()
        .map(|(index, title)| WindowInfo {
            id: WindowId(format!("preview-window-{index}")),
            title: title.into(),
            process_id: None,
            process_name: Some("Example application".into()),
        })
        .collect();
    model.snapshot.audio_sources = [
        (
            "preview-mic",
            "USB microphone",
            AudioSourceKind::Microphone,
            AudioRoute::Ignore,
        ),
        (
            "preview-system",
            "System output",
            AudioSourceKind::System,
            AudioRoute::MainMix,
        ),
        (
            "preview-app",
            "Music application",
            AudioSourceKind::Application,
            AudioRoute::MainMixAndSeparate,
        ),
    ]
    .into_iter()
    .map(|(id, name, kind, route)| AudioSource {
        controls: Default::default(),
        capture_identity: None,
        muted: false,
        id: AudioSourceId(id.into()),
        kind,
        display_name: name.into(),
        process_id: None,
        app_identifier: None,
        is_active: true,
        route,
    })
    .collect();
    model.snapshot.capabilities = EngineCapabilities {
        can_record: true,
        can_pause: true,
        can_replay: true,
        camera_overlay: true,
        audio_levels: true,
        live_preview: true,
        fixed_fps: true,
        display_capture: true,
        window_capture: true,
        region_capture: true,
        per_app_audio: true,
        microphone: true,
        reason: None,
        ..EngineCapabilities::unavailable()
    };
    model.request = Some(RecordingRequest {
        target: CaptureTarget::Display(model.snapshot.displays[0].id.clone()),
        fps_mode: FpsMode::AutoMatchSource,
        resolution: Resolution::Native,
        quality: Quality::Standard,
        encoder: EncoderPreference::Auto,
        optimization: None,
        audio_routes: model
            .snapshot
            .audio_sources
            .iter()
            .map(|s| AudioRouteSelection {
                controls: Default::default(),
                muted: false,
                source_id: s.id.clone(),
                route: s.route,
            })
            .collect(),
        camera: None,
        show_cursor: true,
        output: OutputSettings {
            directory: std::env::temp_dir()
                .join("freym-ui-preview")
                .join("example-recordings")
                .to_string_lossy()
                .into_owned(),
            file_stem: "Preview".into(),
        },
    });
    model.cameras = vec![CameraInfo {
        id: "preview-camera".into(),
        name: "Example camera".into(),
    }];
    model.formats = vec![CameraFormat {
        index: 0,
        width: 1920,
        height: 1080,
        frame_rate_num: 30,
        frame_rate_den: 1,
        subtype: "NV12".into(),
        supported: true,
    }];
    model.sheet = match scene {
        Scene::Settings => Some(Sheet::Settings),
        Scene::Sources => Some(Sheet::Sources),
        Scene::Audio => Some(Sheet::Audio),
        Scene::Camera => Some(Sheet::Camera),
        Scene::Recovery => Some(Sheet::Recovery),
        _ => None,
    };
    model.snapshot.state = match scene {
        Scene::Countdown => RecorderState::Preparing,
        Scene::Recording => RecorderState::Recording,
        Scene::Paused => RecorderState::Paused,
        Scene::Stopping => RecorderState::Finalizing,
        Scene::Error => RecorderState::Failed(RecorderError::unsupported("Example recording")),
        _ => RecorderState::Idle,
    };
    if scene == Scene::Camera {
        model.request.as_mut().unwrap().camera = Some(example_camera("preview-camera".into()));
    }
    if scene == Scene::Error {
        model.error = Some(
            "Example error: the source became unavailable. Choose a source and try again.".into(),
        );
    }
    if scene == Scene::Recovery {
        model.interrupted = vec!["example-interrupted.mkv".into()];
    }
    if matches!(scene, Scene::Recording | Scene::Paused | Scene::Stopping) {
        model.snapshot.metrics = Some(example_metrics());
    }
    model
}
fn example_camera(device_id: String) -> CameraSelection {
    CameraSelection {
        device_id,
        format_index: 0,
        placement: CameraPlacement {
            x: 2100,
            y: 1080,
            width: 320,
            height: 180,
        },
    }
}
fn example_metrics() -> RecorderMetrics {
    RecorderMetrics {
        frame_rate: FrameRateStatus {
            requested_fps: None,
            source_rate_hz: Some(165.),
            actual_capture_fps: Some(165.),
            encoded_fps: Some(165.),
            dropped_frames: Some(0),
            ..FrameRateStatus::default()
        },
        elapsed_ms: 83_000.,
        capture_latency_ms: Some(1.2),
        encode_latency_ms: Some(2.4),
        mux_latency_ms: Some(0.3),
        queues: vec![],
        cpu_percent: Some(3.2),
        gpu_percent: Some(4.5),
        memory_bytes: Some(160_000_000.),
        av_offset_ms: Some(0.),
        audio_underruns: Some(0),
        disk_bytes_per_second: Some(4_000_000.),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext, size};
    #[gpui::test]
    fn preview_interactions_only_change_example_state(cx: &mut TestAppContext) {
        cx.update(crate::bind_keys);
        let (view, cx) = cx.add_window_view(|window, cx| {
            cx.set_reduce_motion(true);
            let mut home = Home::new_ui_preview(window, cx);
            home.model.preferences.artwork = false;
            home
        });
        cx.simulate_resize(size(px(1280.), px(850.)));
        cx.run_until_parked();
        view.update(cx, |home, cx| {
            home.model.sheet = Some(Sheet::Audio);
            cx.notify();
        });
        cx.run_until_parked();
        let point = cx.debug_bounds("audio-mode-advanced").unwrap().center();
        cx.simulate_click(point, Modifiers::default());
        cx.run_until_parked();
        let point = cx
            .debug_bounds("audio-matrix-main-preview-mic")
            .unwrap()
            .center();
        cx.simulate_click(point, Modifiers::default());
        cx.run_until_parked();
        for (route, id) in [
            (
                AudioRoute::MainMixAndSeparate,
                "audio-matrix-own-preview-mic",
            ),
            (AudioRoute::SeparateOnly, "audio-matrix-main-preview-mic"),
            (AudioRoute::Ignore, "audio-matrix-own-preview-mic"),
        ] {
            let point = cx.debug_bounds(id).unwrap().center();
            cx.simulate_click(point, Modifiers::default());
            cx.run_until_parked();
            assert!(
                cx.debug_bounds(id).is_some(),
                "The native matrix remains mounted after selection"
            );
            cx.update(|_, cx| {
                let home = view.read(cx);
                assert_eq!(home.model.snapshot.audio_sources[0].route, route);
                assert_eq!(
                    home.model.request.as_ref().unwrap().audio_routes[0].route,
                    route
                );
                assert_eq!(home.services.control.published().state, RecorderState::Idle);
            });
        }
        view.update(cx, |home, cx| {
            home.model.sheet = None;
            cx.notify();
        });
        cx.run_until_parked();
        let point = cx.debug_bounds("mic-toggle").unwrap().center();
        cx.simulate_click(point, Modifiers::default());
        cx.run_until_parked();
        let point = cx.debug_bounds("record-button").unwrap().center();
        cx.simulate_click(point, Modifiers::default());
        cx.run_until_parked();
        cx.update(|_, cx| {
            let home = view.read(cx);
            assert_eq!(home.model.snapshot.state, RecorderState::Recording);
            assert_eq!(
                home.model.snapshot.audio_sources[0].route,
                AudioRoute::MainMix
            );
            assert_eq!(home.services.control.published().state, RecorderState::Idle);
            assert!(home.services.control.published().audio_sources.is_empty());
            assert!(
                home.hotkeys.is_none() && home.meter_task.is_none() && home.save_latest.is_none()
            );
        });
    }
    #[gpui::test]
    fn workbench_switches_real_home_sheets_and_disables_finalizing(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|window, cx| {
            cx.set_reduce_motion(true);
            let workbench = PreviewWorkbench::new(window, cx);
            workbench
                .home
                .update(cx, |home, _| home.model.preferences.artwork = false);
            workbench
        });
        cx.simulate_resize(size(px(1280.), px(940.)));
        cx.run_until_parked();
        for (id, opener, sheet) in [
            (
                "source-settings",
                Some("inline-source-picker"),
                Sheet::Sources,
            ),
            (
                "camera-settings",
                Some("inline-camera-settings"),
                Sheet::Camera,
            ),
            (
                "quality-settings",
                Some("inline-quality-settings"),
                Sheet::Quality,
            ),
            ("more-options", None, Sheet::Options),
        ] {
            let point = cx.debug_bounds(id).unwrap().center();
            cx.simulate_click(point, Modifiers::default());
            cx.run_until_parked();
            if let Some(opener) = opener {
                let point = cx.debug_bounds(opener).unwrap().center();
                cx.simulate_click(point, Modifiers::default());
                cx.run_until_parked();
            }
            assert_eq!(
                cx.update(|_, cx| view.read(cx).home.read(cx).model.sheet),
                Some(sheet)
            );
            let point = cx.debug_bounds("CloseSheet").unwrap().center();
            cx.simulate_click(point, Modifiers::default());
            cx.run_until_parked();
        }
        for scene in [
            Scene::Settings,
            Scene::Sources,
            Scene::Audio,
            Scene::Camera,
            Scene::Recovery,
            Scene::Stopping,
            Scene::Home,
        ] {
            let point = cx.debug_bounds(scene.id()).unwrap().center();
            cx.simulate_click(point, Modifiers::default());
            cx.run_until_parked();
            cx.update(|_, cx| {
                let home = view.read(cx).home.read(cx);
                assert_eq!(home.model.sheet, example_model(scene).sheet);
                assert_eq!(
                    home.model.snapshot.state,
                    example_model(scene).snapshot.state
                );
                assert_eq!(home.services.control.published().state, RecorderState::Idle);
            });
            if scene == Scene::Stopping {
                let point = cx.debug_bounds("record-button").unwrap().center();
                cx.simulate_click(point, Modifiers::default());
                cx.run_until_parked();
                cx.update(|_, cx| {
                    assert_eq!(
                        view.read(cx).home.read(cx).model.snapshot.state,
                        RecorderState::Finalizing
                    )
                });
            }
        }
    }
}
