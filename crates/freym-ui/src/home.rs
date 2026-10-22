use crate::{
    meter::AudioMeter,
    services::Services,
    state::{AppModel, Preferences, RecorderSection, Sheet, SourceMode},
};
use frame_contracts::*;
use freym_platform::hotkeys::{Hotkeys, ShortcutAction};
use gpui::{
    AnyElement, Context, Entity, FocusHandle, PathPromptOptions, Window, actions, prelude::*, px,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
mod audio;
mod camera;
mod compact;
mod configuration;
mod lifecycle;
#[cfg(test)]
mod motion_tests;
mod persistence;
mod recording_settings;
mod replay;
#[path = "screens/recorder_home.rs"]
mod screen;
#[path = "screens/settings.rs"]
mod sheets;
mod shell;
mod sources;
mod studio;
#[cfg(test)]
mod tests;
#[cfg(feature = "ui-preview")]
pub(crate) mod ui_preview;
mod updates;
mod video;
#[path = "screens/video_settings.rs"]
mod video_screen;

actions!(
    freym,
    [Record, PauseResume, Cancel, OpenSettings, Fullscreen]
);
#[derive(Clone, Debug, PartialEq, Eq)]
enum Action {
    Video(crate::video::VideoAction),
    ToggleSection(RecorderSection),
    Source(SourceMode),
    OpenSourcePicker,
    SelectDisplay(DisplayId),
    SelectWindow(WindowId),
    SelectRoute(AudioSourceId, AudioRoute),
    ToggleAudio(AudioSourceId),
    MuteAudio(AudioSourceId, bool),
    AudioSettings(crate::ui::components::AudioSettingsEvent),
    UseSupportedAudio,
    SelectCamera(String),
    FindCameras,
    SelectCameraFormat(u32),
    RemoveCamera,
    RecoverFile(Option<String>),
    AutomaticQuality,
    SelectQuality(Quality),
    Record,
    Pause,
    Settings,
    QualityOptions,
    MoreOptions,
    Output,
    Audio,
    TestLevels,
    Refresh,
    Recovery,
    Motion,
    Cursor,
    Countdown,
    CloseSheet,
    Replay,
    ToggleReplay,
    SaveReplay,
    PickWindow,
    Shortcut(ShortcutAction),
    Fps,
    ReplayMemory,
    RevealRecording,
    Diagnostics,
    RecordingFilename,
    VerifyRecording,
    CameraPlacement,
    ApplyNumber,
    AutoFps,
    CancelNumber,
    RetrySettings,
    RollbackSettings,
    SettingsCategory(usize),
    Preview,
    Studio,
    StudioRecording,
    StudioReplay,
    InteractionMetadata,
    Compact,
    AutomaticUpdates,
    CheckUpdates,
    Theme(crate::ui::theme::ThemeId),
    LowCostEffects,
    FilledButtons,
}
#[derive(Clone, Copy)]
enum NumberPurpose {
    Fps,
    Bitrate,
    VideoQp,
    VideoGop,
    CameraPlacement,
    ReplayMemory,
    RecordingFilename,
}
struct NumberEditor {
    purpose: NumberPurpose,
    fields: Vec<(&'static str, Entity<crate::input::TextInput>)>,
    slider: Option<Entity<crate::ui::components::motion_slider::MotionSlider>>,
}
#[derive(Default)]
struct StartIntent {
    cancelled: AtomicBool,
    admitted: AtomicBool,
    gate: Mutex<()>,
}
pub struct Home {
    video: crate::video::VideoUiState,
    #[cfg(feature = "ui-preview")]
    ui_preview: bool,
    model: AppModel,
    services: Arc<Services>,
    focus: FocusHandle,
    mic: Entity<AudioMeter>,
    system: Entity<AudioMeter>,
    meter_task: Option<gpui::Task<()>>,
    save_running: bool,
    save_latest: Option<(Option<frame_settings::model::SettingsDocument>, Preferences)>,
    settings_document: Option<frame_settings::model::SettingsDocument>,
    settings_committed: Option<frame_settings::model::SettingsDocument>,
    settings_category: usize,
    pub render_count: u64,
    hotkeys: Option<Hotkeys>,
    shortcut_edit: Option<ShortcutAction>,
    number_editor: Option<NumberEditor>,
    #[cfg(target_os = "windows")]
    preview: Option<Entity<crate::preview::Preview>>,
    preview_enabled: bool,
    studios:
        std::collections::HashMap<std::path::PathBuf, gpui::WindowHandle<crate::studio::Studio>>,
    studio_prompt_pending: bool,
    owner_window: gpui::AnyWindowHandle,
    start_intent: Option<Arc<StartIntent>>,
    compact: Option<gpui::WindowHandle<compact::CompactBar>>,
    compact_position: Option<(gpui::DisplayId, gpui::Bounds<gpui::Pixels>)>,
    pending_mute: Option<(AudioSourceId, bool)>,
    audio_ui: audio::AudioUi,
    updates: Option<Arc<freym_update::Updater>>,
    update_snapshot: freym_update::Snapshot,
    update_running: bool,
    update_monitor: Option<gpui::Task<()>>,
    preferences_loaded: bool,
}
impl Home {
    pub(crate) fn raw_owner(window: &Window) -> isize {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        match HasWindowHandle::window_handle(window).map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
            _ => 0,
        }
    }
}
fn format_elapsed(ms: f64) -> String {
    let s = (ms.max(0.) / 1000.) as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}
impl Home {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (services, receiver) = Services::new();
        let (key_sender, key_receiver) = async_channel::bounded(8);
        let hotkeys = Hotkeys::new(move |id| {
            let _ = key_sender.try_send(id);
        });
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        #[cfg(target_os = "windows")]
        let preview =
            cx.new(|cx| crate::preview::Preview::new(services.control.clone(), window, cx));
        #[cfg(target_os = "windows")]
        cx.observe(&preview, |_, _, cx| cx.notify()).detach();
        let this = Self {
            video: Default::default(),
            #[cfg(feature = "ui-preview")]
            ui_preview: false,
            model: AppModel::default(),
            services: services.clone(),
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
            hotkeys: hotkeys.ok(),
            shortcut_edit: None,
            number_editor: None,
            #[cfg(target_os = "windows")]
            preview: Some(preview),
            preview_enabled: true,
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
            preferences_loaded: false,
        };
        let view = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            let _ = view.update(cx, |this, cx| this.close(window, cx));
            false
        });
        cx.observe_window_bounds(window, |_, _, cx| cx.notify())
            .detach();
        cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() && this.compact.is_some() {
                this.restore_compact(window, cx);
            }
        })
        .detach();
        cx.spawn_in(window, async move |view, cx| {
            while let Ok(id) = key_receiver.recv().await {
                if view
                    .update_in(cx, |this, window, cx| {
                        if this.model.closing
                            || this.model.shortcuts_pending
                            || this.shortcut_edit.is_some()
                        {
                            return;
                        }
                        match this.hotkeys.as_ref().and_then(|h| h.action(id)) {
                            Some(ShortcutAction::StartStop) => this.record(window, cx),
                            Some(ShortcutAction::PauseResume) => this.pause(window, cx),
                            Some(ShortcutAction::SaveReplay) => this.save_replay(cx),
                            None => (),
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        cx.spawn_in(window, async move |view, cx| {
            while receiver.recv().await.is_ok() {
                if view
                    .update_in(cx, |this, window, cx| {
                        let before = this.model.snapshot.state.clone();
                        let had_route = this.model.pending_route.is_some();
                        let previous_sources = this.model.snapshot.audio_sources.clone();
                        this.audio_ui.remember(&previous_sources);
                        let last_replay = this.model.snapshot.last_replay.clone();
                        let last_recording = this.model.snapshot.last_artifact.clone();
                        let replay_settings = this.model.snapshot.replay_settings;
                        this.model
                            .apply_published(this.services.control.published());
                        if this
                            .services
                            .sink
                            .replay_save_finished
                            .swap(false, Ordering::AcqRel)
                        {
                            this.model.replay_save_pending = false;
                            this.model.notice = None;
                        }
                        if last_replay != this.model.snapshot.last_replay
                            && let Some(replay) = &this.model.snapshot.last_replay
                        {
                            this.model.preferences.last_replay = Some(replay.artifact.path.clone());
                            this.model.notice = Some(format!(
                                "Replay saved · {}",
                                std::path::Path::new(&replay.artifact.path)
                                    .file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                            ));
                            this.persist(cx);
                        }
                        if last_recording != this.model.snapshot.last_artifact
                            && let Some(artifact) = &this.model.snapshot.last_artifact
                        {
                            this.model.preferences.last_recording = Some(artifact.path.clone());
                            this.persist(cx);
                        }
                        if replay_settings != this.model.snapshot.replay_settings
                            && !this.model.connecting
                        {
                            this.model.preferences.replay_settings =
                                this.model.snapshot.replay_settings;
                            this.persist(cx);
                        }
                        if let Some(error) = this
                            .services
                            .sink
                            .error
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .take()
                        {
                            this.model.error = Some(error);
                            this.model.command_pending = false;
                            this.model.pending_route = None;
                            this.pending_mute = None;
                        }
                        if before != this.model.snapshot.state {
                            this.pending_mute = None;
                            if this.model.snapshot.state != RecorderState::Preparing {
                                this.start_intent = None;
                            }
                            this.model.command_pending = false;
                            this.model.level_testing = false;
                            this.update_meter_task(window, cx);
                            if this.model.snapshot.state == RecorderState::Idle {
                                this.scan_recovery(cx);
                            }
                        }
                        this.model.sync_routes();
                        this.audio_ui.remember(&this.model.snapshot.audio_sources);
                        this.advance_audio_routes(cx);
                        if matches!(
                            this.model.snapshot.state,
                            RecorderState::Idle | RecorderState::Failed(_)
                        ) {
                            this.flush_save(cx);
                        }
                        if this.pending_mute.as_ref().is_some_and(|(id, muted)| {
                            this.model
                                .snapshot
                                .audio_sources
                                .iter()
                                .find(|s| &s.id == id)
                                .is_none_or(|s| s.muted == *muted)
                        }) {
                            this.pending_mute = None;
                        }
                        let warnings = std::mem::take(
                            &mut *this
                                .services
                                .sink
                                .warnings
                                .lock()
                                .unwrap_or_else(|p| p.into_inner()),
                        );
                        for warning in warnings {
                            if warning.code == "audioLevelTestFailed" {
                                this.model.level_testing = false;
                                this.update_meter_task(window, cx);
                            }
                            this.model
                                .snapshot
                                .warnings
                                .retain(|w| w.code != warning.code);
                            this.model.notice = Some(if warning.code == "audioLevelTestFailed" {
                                format!("Audio test stopped. {}", warning.message)
                            } else {
                                format!("{} {}", warning.message, warning.action)
                            });
                            this.model.snapshot.warnings.push(warning);
                        }
                        if ((had_route && this.model.pending_route.is_none())
                            || previous_sources != this.model.snapshot.audio_sources)
                            && this.audio_ui.routes.is_empty()
                            && this.model.pending_route.is_none()
                            && this.model.error.is_none()
                        {
                            this.persist(cx);
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let init = cx.background_executor().spawn(async move {
            let mut snapshot = frame_runtime::discovery::snapshot()
                .map_err(|e| format!("{} {}", e.message, e.action));
            let document = services.store.load_document().map_err(|e| e.to_string());
            let saved = document.as_ref().map_err(Clone::clone).and_then(|doc| {
                doc.as_ref()
                    .map(|d| d.configuration().map(|c| c.request))
                    .transpose()
            });
            #[cfg(target_os = "windows")]
            if let (Ok(snapshot), Ok(Some(request))) = (&mut snapshot, &saved)
                && let CaptureTarget::Window(id) = &request.target
                && !snapshot.windows.iter().any(|w| &w.id == id)
                && let Ok(window) = frame_windows::validate_window_selection(id)
            {
                snapshot.windows.push(window);
            }
            if let Ok(Some(r)) = &saved {
                services.control.restore_routes(&r.audio_routes);
            }
            let prefs = services.preferences().and_then(|prefs| {
                #[cfg(target_os = "windows")]
                services
                    .control
                    .configure_replay(prefs.replay_settings)
                    .map_err(|error| error.message)?;
                Ok(prefs)
            });
            (
                snapshot,
                saved,
                document,
                services.default_output(),
                prefs,
                services.shortcut_bindings(),
            )
        });
        cx.spawn_in(window, async move |view, cx| {
            let (snapshot, saved, document, output, prefs, shortcuts) = init.await;
            let _ = view.update_in(cx, |this, _, cx| {
                this.model.settings_blocked = saved.is_err();
                let mut errors = Vec::new();
                let saved = match saved {
                    Ok(v) => v,
                    Err(e) => {
                        errors.push(e);
                        None
                    }
                };
                let output = match output {
                    Ok(v) => v,
                    Err(e) => {
                        errors.push(e);
                        String::new()
                    }
                };
                match snapshot {
                    Ok(mut snapshot) => {
                        if let Some(request) = &saved {
                            audio::retain_saved_sources(&mut snapshot.audio_sources, &request.audio_routes);
                        }
                        let p = this
                            .services
                            .control
                            .observe_sources(snapshot.audio_sources.clone());
                        this.model.initialize(snapshot, p, saved, output);
                    }
                    Err(e) => {
                        errors.push(e);
                        this.model.connecting = false;
                    }
                }
                match prefs {
                    Ok(mut p) => {
                        let appearance_migrated = p.migrate_appearance();
                        this.preferences_loaded = true;
                        p.theme.activate();
                        crate::ui::theme::set_low_cost_effects(p.low_cost_effects);
                        crate::ui::theme::set_filled_buttons(p.filled_buttons);
                        cx.refresh_windows();
                        cx.set_reduce_motion(p.reduced_motion || freym_platform::reduced_motion());
                        this.model.preferences = p;
                        if appearance_migrated { this.persist_appearance(cx); }
                        this.start_updates(cx);
                    }
                    Err(e) => {
                        this.update_snapshot.state = freym_update::State::Failed(
                            "Saved preferences could not be read; automatic updates are suspended".into(),
                        );
                        errors.push(e);
                    }
                }
                if let Ok(document) = document {
                    this.install_settings_document(document);
                }
                match shortcuts {
                    Ok(bindings) => {
                        if let Some(hotkeys) = &mut this.hotkeys {
                            match hotkeys.configure(&bindings) {
                                Ok(()) => this.model.shortcuts = bindings,
                                Err(e) => errors.push(e),
                            }
                        } else if bindings != ShortcutBindings::default() {
                            errors.push("Native global shortcuts are unavailable. Use the focused window shortcuts.".into());
                        }
                    }
                    Err(e) => errors.push(e),
                }
                if !errors.is_empty() {
                    this.model.error = Some(errors.join("\n"));
                }
                this.schedule_calibration();
                this.scan_recovery(cx);
                cx.notify();
            });
        })
        .detach();
        this
    }
    fn dispatch(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(action, Action::ApplyNumber)
            && self.number_editor.as_ref().is_some_and(|e| {
                matches!(
                    e.purpose,
                    NumberPurpose::Fps
                        | NumberPurpose::Bitrate
                        | NumberPurpose::VideoQp
                        | NumberPurpose::VideoGop
                )
            })
        {
            self.apply_video_number(window, cx);
            return;
        }
        #[cfg(feature = "ui-preview")]
        if self.ui_preview && self.dispatch_preview(&action, window, cx) {
            cx.notify();
            return;
        }
        match action {
            Action::Video(action) => self.dispatch_video(action, window, cx),
            Action::Theme(theme) if self.preferences_loaded && !self.model.closing => {
                self.model.preferences.theme = theme;
                if !theme.is_motion()
                    && let Some(editor) = &mut self.number_editor
                {
                    editor.slider = None;
                }
                theme.activate();
                cx.refresh_windows();
                self.persist_appearance(cx);
            }
            Action::LowCostEffects if self.preferences_loaded && !self.model.closing => {
                self.model.preferences.low_cost_effects = !self.model.preferences.low_cost_effects;
                crate::ui::theme::set_low_cost_effects(self.model.preferences.low_cost_effects);
                cx.refresh_windows();
                self.persist_appearance(cx);
            }
            Action::FilledButtons if self.preferences_loaded && !self.model.closing => {
                self.model.preferences.filled_buttons = !self.model.preferences.filled_buttons;
                crate::ui::theme::set_filled_buttons(self.model.preferences.filled_buttons);
                cx.refresh_windows();
                self.persist_appearance(cx);
            }
            Action::Compact => self.enter_compact(window, cx),
            Action::AutomaticUpdates => self.toggle_updates(cx),
            Action::CheckUpdates => self.check_updates(true, cx),
            Action::Studio => self.open_studio(cx),
            Action::StudioRecording => {
                if let Some(path) = self
                    .model
                    .snapshot
                    .last_artifact
                    .as_ref()
                    .map(|a| a.path.clone())
                    .or_else(|| self.model.preferences.last_recording.clone())
                {
                    self.open_studio_path(path.into(), cx);
                }
            }
            Action::StudioReplay => {
                if let Some(path) = self
                    .model
                    .snapshot
                    .last_replay
                    .as_ref()
                    .map(|r| r.artifact.path.clone())
                    .or_else(|| self.model.preferences.last_replay.clone())
                {
                    self.open_studio_path(path.into(), cx);
                }
            }
            Action::InteractionMetadata if self.model.editable() => {
                self.model.preferences.interaction_metadata =
                    !self.model.preferences.interaction_metadata;
                self.persist(cx);
            }
            Action::ToggleSection(section) if !self.model.closing => {
                self.model.preferences.recorder_sections.toggle(section);
                self.persist_appearance(cx);
            }
            Action::FindCameras if self.model.editable() => self.load_cameras(cx),
            Action::OpenSourcePicker if self.model.editable() => {
                self.model.mode = match self.model.request.as_ref().map(|r| &r.target) {
                    Some(CaptureTarget::Window(_)) => SourceMode::Window,
                    Some(CaptureTarget::Region(_)) => SourceMode::Area,
                    _ => SourceMode::Screen,
                };
                self.model.sheet = Some(Sheet::Sources)
            }
            Action::SelectDisplay(id) if self.model.editable() => {
                if let Some(display) = self
                    .model
                    .snapshot
                    .displays
                    .iter()
                    .find(|d| d.id == id)
                    .cloned()
                {
                    if self.model.mode == SourceMode::Area {
                        self.pick_area(display, window, cx);
                        return;
                    }
                    if let Some(request) = &mut self.model.request {
                        request.target = CaptureTarget::Display(id);
                        if let Some(quality) = display.required_recording_quality {
                            request.quality = quality;
                        }
                    }
                    self.model.mode = SourceMode::Screen;
                    self.model.sheet = None;
                    self.persist(cx);
                }
            }
            Action::SelectWindow(id) => self.choose_window(id, cx),
            Action::SelectRoute(id, route) => self.route(id, route, cx),
            Action::AudioSettings(event) => self.audio_settings_event(event, window, cx),
            Action::ToggleAudio(id) if self.model.editable() => {
                if let Some(source) = self
                    .model
                    .snapshot
                    .audio_sources
                    .iter()
                    .find(|s| s.id == id)
                {
                    self.route(
                        id,
                        if source.route == AudioRoute::Ignore {
                            AudioRoute::MainMix
                        } else {
                            AudioRoute::Ignore
                        },
                        cx,
                    );
                }
            }
            Action::MuteAudio(id, muted) => {
                if self.can_mute(&id) {
                    match self.services.control.update_mute(id.clone(), muted) {
                        Ok(()) => self.pending_mute = Some((id, muted)),
                        Err(error) => self.error(error),
                    }
                }
            }
            Action::SelectCamera(id) if self.model.editable() => self.select_camera(id, cx),
            Action::SelectCameraFormat(index) if self.model.editable() => {
                if let Some(camera) = self.model.request.as_mut().and_then(|r| r.camera.as_mut()) {
                    camera.format_index = index;
                }
                self.persist(cx);
            }
            Action::RemoveCamera if self.model.editable() => {
                if let Some(request) = &mut self.model.request {
                    request.camera = None;
                }
                self.persist(cx);
            }
            Action::RecoverFile(path) => self.recover(path, cx),
            Action::AutomaticQuality if self.model.editable() => {
                self.model.set_automatic_quality();
                self.persist(cx);
            }
            Action::SelectQuality(quality) if self.model.editable() => {
                if self.model.set_quality(quality) {
                    self.persist(cx);
                }
            }
            Action::Source(mode) if self.model.editable() => {
                self.model.mode = mode;
                self.model.sheet = Some(if mode == SourceMode::Camera {
                    Sheet::Camera
                } else {
                    Sheet::Sources
                });
                if mode == SourceMode::Camera {
                    self.load_cameras(cx);
                }
            }
            Action::Record => self.record(window, cx),
            Action::Preview => self.open_preview(window, cx),
            Action::RetrySettings => self.reload_settings(cx),
            Action::UseSupportedAudio => {
                if self.model.editable()
                    && let Some(document) = &mut self.settings_document
                    && let Ok(mut c) = document.configuration()
                {
                    c.audio = Default::default();
                    if let Err(e) = document.set_configuration(&c) {
                        self.model.error = Some(e);
                        return;
                    }
                    self.persist(cx);
                }
            }
            Action::RollbackSettings => self.rollback_settings(cx),
            Action::SettingsCategory(category) => self.settings_category = category.min(7),
            Action::Shortcut(action) if !self.model.shortcuts_pending => {
                self.shortcut_edit = Some(action);
                self.model.error = None;
            }
            Action::Fps if self.model.editable() => {
                self.edit_number(NumberPurpose::Fps, window, cx)
            }
            Action::CameraPlacement if self.model.editable() => {
                self.edit_number(NumberPurpose::CameraPlacement, window, cx)
            }
            Action::ReplayMemory if self.model.editable() => {
                self.edit_number(NumberPurpose::ReplayMemory, window, cx)
            }
            Action::RecordingFilename if self.model.editable() => {
                self.edit_number(NumberPurpose::RecordingFilename, window, cx)
            }
            Action::VerifyRecording => self.verify_recording(cx),
            Action::ApplyNumber => self.apply_number(window, cx),
            Action::AutoFps if self.model.editable() => {
                if let Some(r) = &mut self.model.request {
                    r.fps_mode = FpsMode::AutoMatchSource;
                }
                self.number_editor = None;
                window.focus(&self.focus, cx);
                self.persist(cx);
                self.check_video_hardware(false, cx);
            }
            Action::CancelNumber => {
                self.number_editor = None;
                window.focus(&self.focus, cx);
            }
            Action::RevealRecording => {
                if let Some(a) = &self.model.snapshot.last_artifact {
                    cx.reveal_path(std::path::Path::new(&a.path));
                }
            }
            Action::Diagnostics => self.model.sheet = Some(Sheet::Diagnostics),
            Action::Pause => self.pause(window, cx),
            Action::Settings => {
                self.model.sheet = Some(Sheet::Settings);
                self.check_video_hardware(false, cx);
            }
            Action::QualityOptions => {
                self.model.sheet = Some(Sheet::Quality);
                self.check_video_hardware(false, cx);
            }
            Action::MoreOptions => self.model.sheet = Some(Sheet::Options),
            Action::Audio => self.model.sheet = Some(Sheet::Audio),
            Action::Output if self.model.editable() => self.choose_output(cx),
            Action::TestLevels => self.test_levels(window, cx),
            Action::Refresh if self.model.editable() => self.refresh(cx),
            Action::Recovery => {
                self.model.sheet = Some(Sheet::Recovery);
                self.scan_recovery(cx);
            }
            Action::Motion => {
                self.model.preferences.reduced_motion = !self.model.preferences.reduced_motion;
                cx.set_reduce_motion(
                    self.model.preferences.reduced_motion || freym_platform::reduced_motion(),
                );
                self.persist(cx);
            }
            Action::Cursor if self.model.editable() => {
                if let Some(r) = &mut self.model.request {
                    r.show_cursor = !r.show_cursor;
                }
                self.persist(cx);
            }
            Action::Countdown if self.model.editable() => {
                self.model.preferences.countdown = match self.model.preferences.countdown {
                    0 => 3,
                    3 => 5,
                    5 => 10,
                    _ => 0,
                };
                self.persist(cx);
            }
            Action::PickWindow => self.pick_window(window, cx),
            Action::CloseSheet => {
                self.model.sheet = None;
                window.focus(&self.focus, cx);
            }
            Action::Replay => self.configure_replay(cx),
            Action::ToggleReplay => self.toggle_replay(cx),
            Action::SaveReplay => self.save_replay(cx),
            _ => {}
        }
        cx.notify();
    }
}
