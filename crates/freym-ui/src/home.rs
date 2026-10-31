#[cfg(test)]
use crate::state::RecorderSection;
use crate::{
    meter::AudioMeter,
    services::Services,
    state::{AppModel, Preferences, Sheet, SourceMode},
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
mod completion;
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
mod shortcuts;
mod source_menu;
mod sources;
mod startup;
#[cfg(test)]
mod switch_performance_tests;
#[cfg(test)]
mod tests;

mod updates;
mod video;
#[path = "screens/video_settings.rs"]
mod video_screen;

mod workspace;

actions!(
    freym,
    [Record, PauseResume, Cancel, OpenSettings, Fullscreen]
);
#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::enum_variant_names)]
enum Action {
    Video(crate::video::VideoAction),
    #[cfg(test)]
    ToggleSection(RecorderSection),
    Source(SourceMode),
    OpenSourcePicker,
    SelectDisplay(DisplayId),
    SelectTarget(CaptureTarget),
    SelectWindow(WindowId),
    SelectRoute(AudioSourceId, AudioRoute),
    ToggleAudio(AudioSourceId),
    RemoveRecorderAudio(AudioSourceId),
    MuteAudio(AudioSourceId, bool),
    AudioSettings(crate::ui::components::AudioSettingsEvent),
    UseSupportedAudio,
    SelectCamera(String),
    FindCameras,
    CameraDeviceSettings,
    LaunchAtStartup,
    SelectCameraFormat(u32),
    RemoveCamera,
    RecoverFile(Option<String>),
    SelectRecovery(String),
    Record,
    Pause,
    Settings,
    #[cfg(test)]
    QualityOptions,
    Output,
    OpenOutput,
    Audio,
    Refresh,
    Recovery,
    Motion,
    Cursor,
    SetCountdown(u8),
    CloseSheet,
    Replay,
    ToggleReplay,
    SaveReplay,
    PickWindow,
    Shortcut(ShortcutAction),
    ClearShortcut(ShortcutAction),
    RestoreShortcuts,
    ApplicationShortcut(shortcuts::ApplicationShortcut),
    ClearApplicationShortcut(shortcuts::ApplicationShortcut),
    ReplayMemory,
    Diagnostics,
    RecordingFilename,
    VerifyRecording,
    CameraPlacement,
    ApplyNumber,
    AutoFps,
    CancelNumber,
    RetrySettings,
    RetrySave,
    KeepEditing,
    LeaveUnsaved,
    Exit,
    RollbackSettings,
    SettingsCategory(usize),
    InteractionMetadata,
    AutomaticUpdates,
    CheckUpdates,
    Theme(crate::ui::theme::ThemeId),
    Surface(crate::ui::theme::SurfacePreference),
    FrostedTransparency(u8),
    PreviewFrostedTransparency(u8),
    #[cfg(test)]
    Accent(Option<u32>),
    LowCostEffects,
    FilledButtons,
}
#[derive(Clone, Copy)]
enum NumberPurpose {
    Resolution,
    Fps,
    Bitrate,
    VideoQp,
    VideoGop,
    ReplayMemory,
    RecordingFilename,
}
struct NumberEditor {
    purpose: NumberPurpose,
    fields: Vec<(&'static str, Entity<crate::input::TextInput>)>,
}
#[derive(Default)]
struct StartIntent {
    cancelled: AtomicBool,
    admitted: AtomicBool,
    gate: Mutex<()>,
}
pub struct Home {
    video: crate::video::VideoUiState,
    startup: startup::StartupState,
    camera_ui: camera::CameraUi,

    model: AppModel,
    services: Arc<Services>,
    focus: FocusHandle,
    mic: Entity<AudioMeter>,
    system: Entity<AudioMeter>,
    meter_task: Option<gpui::Task<()>>,
    save_running: bool,
    save_state: persistence::SaveState,
    save_latest: Option<(Option<frame_settings::model::SettingsDocument>, Preferences)>,
    settings_document: Option<frame_settings::model::SettingsDocument>,
    settings_committed: Option<frame_settings::model::SettingsDocument>,
    settings_category: usize,
    settings_scroll: gpui::ScrollHandle,
    pub render_count: u64,
    source_shelf_targets: Vec<CaptureTarget>,
    list_preview: Option<shell::ListPreview>,
    workspace_ui: workspace::WorkspaceUi,
    hotkeys: Option<Hotkeys>,
    shortcut_edit: Option<ShortcutAction>,
    shortcut_ui: shortcuts::ShortcutUi,
    number_editor: Option<NumberEditor>,
    #[cfg(target_os = "windows")]
    preview: Option<Entity<crate::preview::Preview>>,
    preview_enabled: bool,
    owner_window: gpui::AnyWindowHandle,
    start_intent: Option<Arc<StartIntent>>,
    pending_mute: Option<(AudioSourceId, bool)>,
    recording_completion: Option<completion::RecordingCompletion>,
    completion_generation: u64,
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
        let shortcut_manager_error = hotkeys.as_ref().err().cloned();
        let mut shortcut_ui = shortcuts::ShortcutUi::default();
        shortcut_ui.manager_error = shortcut_manager_error;
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        #[cfg(target_os = "windows")]
        let preview =
            cx.new(|cx| crate::preview::Preview::new(services.control.clone(), window, cx));
        #[cfg(target_os = "windows")]
        cx.observe(&preview, |home, preview, cx| {
            if home
                .model
                .request
                .as_ref()
                .is_some_and(|r| r.camera.is_some())
                && let Some(warning) = preview.read(cx).camera_warning().map(str::to_owned)
            {
                home.model.notice = Some(format!(
                    "Camera unavailable: {warning}. Desktop capture continues."
                ));
                if let Some(camera) = home.model.request.as_mut().and_then(|r| r.camera.take()) {
                    home.model.preferences.disabled_camera = Some(camera);
                    home.persist(cx);
                }
            }
            cx.notify();
        })
        .detach();
        let mut this = Self {
            video: Default::default(),
            startup: Default::default(),
            camera_ui: Default::default(),

            model: AppModel::default(),
            services: services.clone(),
            focus,
            mic: cx.new(|_| AudioMeter::default()),
            system: cx.new(|_| AudioMeter::default()),
            meter_task: None,
            save_running: false,
            save_state: Default::default(),
            save_latest: None,
            settings_document: None,
            settings_committed: None,
            settings_category: 0,
            settings_scroll: gpui::ScrollHandle::new(),
            render_count: 0,
            source_shelf_targets: vec![],
            list_preview: None,
            workspace_ui: Default::default(),
            hotkeys: hotkeys.ok(),
            shortcut_edit: None,
            shortcut_ui,
            number_editor: None,
            #[cfg(target_os = "windows")]
            preview: Some(preview),
            preview_enabled: true,
            owner_window: window.window_handle(),
            start_intent: None,
            pending_mute: None,
            recording_completion: None,
            completion_generation: 0,
            audio_ui: Default::default(),
            updates: None,
            update_snapshot: Default::default(),
            update_running: false,
            update_monitor: None,
            preferences_loaded: false,
        };
        this.load_startup(cx);
        let view = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            let _ = view.update(cx, |this, cx| this.close(window, cx));
            false
        });
        cx.observe_window_bounds(window, |_, _, cx| cx.notify())
            .detach();
        cx.spawn_in(window, async move |view, cx| {
            while let Ok(id) = key_receiver.recv().await {
                if view
                    .update_in(cx, |this, window, cx| {
                        if this.model.closing
                            || this.model.shortcuts_pending
                            || this.shortcut_capture_active()
                        {
                            return;
                        }
                        match this
                            .hotkeys
                            .as_ref()
                            .and_then(|h| h.action_event(id))
                            .filter(|action| this.shortcut_supported(*action))
                        {
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
                            && let Some(artifact) = this.model.snapshot.last_artifact.clone()
                        {
                            this.model.preferences.last_recording = Some(artifact.path.clone());
                            this.persist(cx);
                            this.present_recording_completion(artifact, window, cx);
                        }
                        // Finalized is published just before Idle; keep its popup
                        // pending across that event boundary.
                        if !matches!(
                            this.model.snapshot.state,
                            RecorderState::Idle | RecorderState::Finalizing
                        ) {
                            this.dismiss_recording_completion(window, cx);
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
                            if matches!(this.model.snapshot.state, RecorderState::Failed(_)) {
                                let services = this.services.clone();
                                let snapshot = this.model.snapshot.clone();
                                let request = this.model.request.clone();
                                let write = cx.background_executor().spawn(async move {
                                    services.save_recording_failure(&snapshot, request.as_ref())
                                });
                                cx.spawn(async move |view, cx| {
                                    if let Err(error) = write.await {
                                        let _ = view.update(cx, |this, cx| {
                                            let message =
                                                "Recording failure details could not be saved.";
                                            let diagnostic = this
                                                .model
                                                .error
                                                .get_or_insert_with(|| message.into());
                                            diagnostic.push_str(&format!(
                                                "\nDiagnostic file could not be saved: {error}"
                                            ));
                                            cx.notify();
                                        });
                                    }
                                })
                                .detach();
                                this.scan_recovery(cx);
                            }
                            if let Some(resources) = &this.workspace_ui.resources {
                                resources.update(cx, |r, cx| {
                                    r.recording(
                                        matches!(
                                            this.model.snapshot.state,
                                            RecorderState::Recording | RecorderState::Paused
                                        ),
                                        cx,
                                    )
                                });
                            }
                            this.pending_mute = None;
                            if this.model.snapshot.state != RecorderState::Preparing {
                                this.start_intent = None;
                            }
                            this.model.command_pending = false;
                            this.model.level_testing = false;
                            this.update_meter_task(window, cx);
                            if this.model.snapshot.state == RecorderState::Idle {
                                this.scan_recovery(cx);
                                this.apply_staged_target(window, cx);
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
                        crate::ui::theme::set_accent(p.accent_color);
                        p.surface.activate();
                        crate::ui::theme::surface_treatment::set_frosted_transparency(p.frosted_transparency);
                        crate::ui::theme::set_low_cost_effects(p.low_cost_effects);
                        crate::ui::theme::set_filled_buttons(p.filled_buttons);
                        cx.refresh_windows();
                        cx.set_reduce_motion(p.reduced_motion || freym_platform::reduced_motion());
                        this.model.preferences = p;
                        #[cfg(target_os="windows")]
                        let _=this.services.control.configure_performance(this.model.preferences.performance);
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
                this.apply_startup_source_defaults(
                    document.as_ref().ok().and_then(Option::as_ref)
                        .is_some_and(|d| d.active_profile.is_some()),
                );
                if let Ok(document) = document {
                    this.install_settings_document(document);
                }
                this.check_video_hardware(false, cx);
                match shortcuts {
                    Ok(bindings) => {
                        this.model.shortcuts = bindings.clone();
                        let native=this.native_shortcuts(&bindings);
                        let registration = match &mut this.hotkeys {
                            Some(hotkeys) => hotkeys.configure(&native),
                            None if bindings != ShortcutBindings::default() => Err("Native global shortcuts are unavailable; saved bindings still work in the focused Recorder window.".into()),
                            None => Ok(()),
                        };
                        if let Err(error) = registration { this.shortcut_ui.error = Some(error.clone()); errors.push(error); }
                    }
                    Err(error) => { this.shortcut_ui.error = Some(error.clone()); errors.push(error); }
                }
                if !errors.is_empty() {
                    this.model.error = Some(errors.join("\n"));
                }
                this.save_state.shortcuts = this.model.shortcuts.clone();
                this.save_latest = None;
                this.schedule_calibration();
                this.scan_recovery(cx);
                cx.notify();
            });
        })
        .detach();
        this
    }
    fn dispatch(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.sheet == Some(Sheet::Settings)
            && self.has_unsaved_settings()
            && matches!(
                action,
                Action::CloseSheet | Action::CameraPlacement | Action::RetrySettings
            )
            && !self.save_state.leaving
        {
            self.save_state.leave = Some(action);
            cx.notify();
            return;
        }
        if matches!(
            self.model.snapshot.state,
            RecorderState::Recording | RecorderState::Paused
        ) {
            let target = match &action {
                Action::SelectDisplay(id) => Some(CaptureTarget::Display(id.clone())),
                Action::SelectWindow(id) => Some(CaptureTarget::Window(id.clone())),
                _ => None,
            };
            if let Some(target) = target {
                self.select_capture_target(target, window, cx);
                return;
            }
        }
        self.save_state.leaving = false;
        // Navigation uses the exhaustive preview dispatcher below. Recording and
        // replay remain blocked even when the inspector is hidden.

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

        match action {
            Action::Video(action) => self.dispatch_video(action, window, cx),
            Action::SelectTarget(target) => self.select_capture_target(target, window, cx),
            Action::ApplicationShortcut(action) => {
                self.begin_application_shortcut(action, window, cx)
            }
            Action::ClearApplicationShortcut(action) => {
                self.set_application_binding(action, String::new(), cx);
            }
            Action::Theme(theme) if self.preferences_loaded && !self.model.closing => {
                self.model.preferences.theme = theme;
                theme.activate();
                cx.refresh_windows();
                self.persist_appearance(cx);
            }
            #[cfg(test)]
            Action::Accent(color) if self.preferences_loaded && !self.model.closing => {
                self.model.preferences.accent_color = color.filter(|v| *v <= 0xffffff);
                crate::ui::theme::set_accent(self.model.preferences.accent_color);
                cx.refresh_windows();
                self.persist_appearance(cx);
            }
            Action::LowCostEffects if self.preferences_loaded && !self.model.closing => {
                self.model.preferences.low_cost_effects = !self.model.preferences.low_cost_effects;
                crate::ui::theme::set_low_cost_effects(self.model.preferences.low_cost_effects);
                cx.refresh_windows();
                self.persist_appearance(cx);
            }
            Action::Surface(surface) if self.preferences_loaded && !self.model.closing => {
                self.model.preferences.surface = surface;
                surface.activate();
                cx.refresh_windows();
                self.persist_appearance(cx);
            }
            Action::PreviewFrostedTransparency(percent)
                if self.preferences_loaded
                    && !self.model.closing
                    && crate::ui::theme::surface_treatment::is_frosted() =>
            {
                let percent = percent.min(100);
                if self.model.preferences.frosted_transparency != percent {
                    self.model.preferences.frosted_transparency = percent;
                    crate::ui::theme::surface_treatment::set_frosted_transparency(percent);
                    // The normal Home notify below coalesces this repaint. There
                    // are no cached surface views here; no full-window refresh or
                    // preference write is needed for each pointer value.
                }
            }
            Action::FrostedTransparency(percent)
                if self.preferences_loaded && !self.model.closing =>
            {
                let percent = percent.min(100);
                self.model.preferences.frosted_transparency = percent;
                crate::ui::theme::surface_treatment::set_frosted_transparency(percent);
                cx.refresh_windows();
                self.persist_appearance(cx);
            }
            Action::FilledButtons if self.preferences_loaded && !self.model.closing => {
                self.model.preferences.filled_buttons = !self.model.preferences.filled_buttons;
                crate::ui::theme::set_filled_buttons(self.model.preferences.filled_buttons);
                cx.refresh_windows();
                self.persist_appearance(cx);
            }
            Action::AutomaticUpdates => self.toggle_updates(cx),
            Action::CheckUpdates => self.check_updates(true, cx),
            Action::LaunchAtStartup => self.toggle_startup(cx),
            Action::CameraDeviceSettings => self.open_camera_settings(cx),
            Action::InteractionMetadata if self.model.editable() => {
                self.model.preferences.interaction_metadata =
                    !self.model.preferences.interaction_metadata;
                self.persist(cx);
            }
            #[cfg(test)]
            Action::ToggleSection(section) if !self.model.closing => {
                self.model.preferences.recorder_sections.toggle(section);
                self.persist_appearance(cx);
            }
            Action::FindCameras if self.model.editable() => self.load_cameras(cx),
            Action::OpenSourcePicker if self.panels_interactive() => {
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
                        if request.target != CaptureTarget::Display(id.clone()) {
                            request.resolution = Resolution::Native;
                        }
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
            Action::RemoveRecorderAudio(id) => self.remove_recorder_audio_source(id, cx),
            Action::AudioSettings(event) => self.audio_settings_event(event, window, cx),
            Action::RetrySave => self.retry_save(cx),
            Action::KeepEditing => self.save_state.leave = None,
            Action::LeaveUnsaved => {
                if let Some(next) = self.save_state.leave.take() {
                    self.save_state.leaving = true;
                    self.dispatch(next, window, cx);
                }
            }
            Action::Exit => {
                self.save_latest = None;
                self.save_state.leave = None;
                self.clear_save_error();
                self.save_state.exit_confirmed = true;
                self.close(window, cx);
            }
            Action::ToggleAudio(id) if self.model.editable() => {
                if let Some(source) = self
                    .model
                    .snapshot
                    .audio_sources
                    .iter()
                    .find(|s| s.id == id)
                {
                    let next = if source.route == AudioRoute::Ignore {
                        self.enabled_audio_route(&id)
                    } else {
                        AudioRoute::Ignore
                    };
                    self.route(id, next, cx);
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
            Action::SelectCameraFormat(index) => self.select_camera_format(index, cx),
            Action::RemoveCamera if self.model.editable() => {
                self.model.preferences.disabled_camera = None;
                self.camera_ui.editing = false;

                self.camera_ui.drag = None;
                if let Some(request) = &mut self.model.request {
                    request.camera = None;
                }
                self.persist(cx);
            }
            Action::RecoverFile(path) => self.recover(path, cx),
            Action::SelectRecovery(path)
                if !self.model.recovery_busy && self.model.interrupted.contains(&path) =>
            {
                self.model.recovery_selected = Some(path);
            }
            Action::Source(mode)
                if self.panels_interactive()
                    && (self.model.editable()
                        || matches!(mode, SourceMode::Screen | SourceMode::Window)) =>
            {
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
            Action::SettingsCategory(category) => {
                if self.settings_category != category.min(8) {
                    self.audio_ui.bitrate_draft = None;
                    self.cancel_shortcut_capture(cx);
                    self.settings_category = category.min(8);
                    self.settings_scroll.set_offset(gpui::point(px(0.), px(0.)));
                }
                if self.settings_category == 6 && !self.camera_ui.loaded {
                    self.load_cameras(cx);
                }
            }
            Action::Shortcut(action) if !self.model.shortcuts_pending => {
                self.begin_shortcut_capture(action, window, cx)
            }
            Action::ClearShortcut(action) => self.clear_shortcut(action, cx),
            Action::RestoreShortcuts => self.restore_shortcuts(cx),
            Action::CameraPlacement if self.model.editable() => {
                self.camera_ui.editing = true;
                self.model.sheet = None;
                window.focus(&self.focus, cx);
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
                self.flush_save(cx);
            }
            Action::Diagnostics => self.model.sheet = Some(Sheet::Diagnostics),
            Action::Pause => self.pause(window, cx),
            Action::Settings => {
                self.model.sheet = Some(Sheet::Settings);
                self.check_video_hardware(false, cx);
            }
            #[cfg(test)]
            Action::QualityOptions => {
                self.model.sheet = Some(Sheet::Quality);
                self.check_video_hardware(false, cx);
            }
            Action::Audio => self.model.sheet = Some(Sheet::Audio),
            Action::Output if self.model.editable() => self.choose_output(cx),
            Action::OpenOutput if !self.model.closing => {
                if let Some(directory) = self.output_directory() {
                    #[cfg(test)]
                    shell::OPENED_OUTPUT
                        .with(|path| *path.borrow_mut() = Some(directory.to_owned()));
                    #[cfg(not(test))]
                    cx.open_with_system(std::path::Path::new(directory));
                }
            }
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
                self.persist_appearance(cx);
            }
            Action::Cursor if self.model.editable() => {
                if let Some(r) = &mut self.model.request {
                    r.show_cursor = !r.show_cursor;
                }
                self.persist(cx);
            }
            Action::SetCountdown(value) if self.model.editable() => {
                if self.model.preferences.countdown != value {
                    self.model.preferences.countdown = value;
                    self.persist(cx);
                }
            }
            Action::PickWindow => self.pick_window(window, cx),
            Action::CloseSheet => {
                self.audio_ui.bitrate_draft = None;
                self.cancel_shortcut_capture(cx);
                self.model.sheet = None;
                window.focus(&self.focus, cx);
                self.flush_save(cx);
            }
            Action::Replay => self.configure_replay(cx),
            Action::ToggleReplay => self.toggle_replay(cx),
            Action::SaveReplay => self.save_replay(cx),
            _ => {}
        }
        cx.notify();
    }
}
