use super::*;
#[cfg(test)]
#[path = "audio/tests.rs"]
mod tests;
use crate::{audio_adapter as routing, ui::components::*};
use std::collections::{BTreeMap, VecDeque};

#[derive(Default)]
pub(super) struct AudioUi {
    pub output_revision: u64,
    pub bitrate_draft: Option<String>,
    pub selected_source: Option<AudioSourceId>,
    pub(super) recorder_microphone: Option<AudioSourceId>,
    pub(super) recorder_system: Option<AudioSourceId>,
    pub(super) order: Vec<AudioSourceId>,
    pub(super) routes: VecDeque<(AudioSourceId, AudioRoute)>,
    known: BTreeMap<AudioSourceId, AudioSource>,
    processing: Option<(AudioSourceId, AudioSourceControls)>,
    pub(super) ribbons: BTreeMap<AudioSourceId, Entity<PulseRibbon>>,
}
impl AudioUi {
    pub(super) fn remember(&mut self, sources: &[AudioSource]) {
        for source in sources {
            if source.route == AudioRoute::Ignore {
                self.known.remove(&source.id);
            } else {
                self.known.insert(source.id.clone(), source.clone());
            }
        }
    }
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct AudioPreferences {
    #[serde(skip_serializing)]
    advanced: bool,
    microphone: Option<AudioSourceId>,
    system: Option<AudioSourceId>,
    separate: bool,
    track_mode: Option<AudioRoute>,
    /// Sources explicitly added to the Recorder; visibility never enables capture.
    recorder_sources: Vec<AudioSourceId>,
    /// Explicitly removed disabled rows; remembered routes/device selectors stay intact.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    recorder_removed: Vec<AudioSourceId>,
    /// Last enabled destination; disabling capture does not choose a new route.
    routes: BTreeMap<AudioSourceId, AudioRoute>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

pub(super) fn retain_saved_sources(sources: &mut Vec<AudioSource>, routes: &[AudioRouteSelection]) {
    for selection in routes.iter().filter(|r| r.route != AudioRoute::Ignore) {
        if !sources.iter().any(|s| s.id == selection.source_id) {
            let id = &selection.source_id.0;
            sources.push(AudioSource {
                controls: selection.controls,
                capture_identity: None,
                id: selection.source_id.clone(),
                kind: if id.starts_with("wasapi-session:")
                    || id.starts_with("app:")
                    || id.starts_with("pulse-app:")
                {
                    AudioSourceKind::Application
                } else if id.starts_with("wasapi-loopback:") || id.starts_with("wasapi-rest:") {
                    AudioSourceKind::System
                } else {
                    AudioSourceKind::Microphone
                },
                display_name: format!("Unavailable source · {id}"),
                process_id: None,
                app_identifier: None,
                is_active: false,
                route: selection.route,
                muted: selection.muted,
            });
        }
    }
}

impl Home {
    pub(super) fn can_mute(&self, id: &AudioSourceId) -> bool {
        !self.model.connecting
            && !self.model.command_pending
            && !self.model.closing
            && self.pending_mute.is_none()
            && matches!(
                self.model.snapshot.state,
                RecorderState::Idle | RecorderState::Recording | RecorderState::Paused
            )
            && self
                .model
                .snapshot
                .audio_sources
                .iter()
                .any(|s| &s.id == id && s.is_active && s.route != AudioRoute::Ignore)
    }

    pub(super) fn apply_audio_output(&mut self, output: AudioSettings, cx: &mut Context<Self>) {
        if !self.model.editable() || self.model.level_testing || self.model.settings_blocked {
            return;
        }
        if let Err(error) = output.validate() {
            self.error(error);
            return;
        }
        if self.settings_document.is_none() {
            self.install_settings_document(None);
        }
        if let Some(document) = &mut self.settings_document {
            let result = document.configuration().and_then(|mut c| {
                c.audio = output;
                c.request.audio_encoding = output;
                document.set_configuration(&c)
            });
            if let Err(error) = result {
                self.model.error = Some(error);
                return;
            }
        } else {
            return;
        }
        if let Some(request) = &mut self.model.request {
            request.audio_encoding = output;
        }
        self.audio_ui.output_revision = self.audio_ui.output_revision.wrapping_add(1);
        self.model.error = None;
        self.persist_mode(cx);
    }
    pub(super) fn ribbon_status(&self, source: &AudioSource) -> RibbonStatus {
        if source.route == AudioRoute::Ignore {
            RibbonStatus::Off
        } else if source.muted {
            RibbonStatus::Muted
        } else if !source.is_active {
            RibbonStatus::Unavailable
        } else if self.model.snapshot.state == RecorderState::Paused {
            RibbonStatus::Paused
        } else {
            RibbonStatus::Live
        }
    }
    pub(super) fn feed_meter_snapshot(
        &mut self,
        levels: &AudioLevelsSnapshot,
        cx: &mut Context<Self>,
    ) {
        let mut mic: (Option<f32>, Option<f32>) = (None, None);
        let mut system: (Option<f32>, Option<f32>) = (None, None);
        for source in &self.model.snapshot.audio_sources {
            let status = self.ribbon_status(source);
            let level = levels.sources.iter().find(|l| l.source_id == source.id);
            if let Some(ribbon) = self.audio_ui.ribbons.get(&source.id) {
                ribbon.update(cx, |ribbon, cx| {
                    ribbon.set_status(status, cx);
                    ribbon.sample(levels.session_id.as_deref(), level, cx);
                });
            }
            if status == RibbonStatus::Live
                && levels.session_id.is_some()
                && let Some(level) = level
            {
                let slot = if source.kind == AudioSourceKind::Microphone {
                    &mut mic
                } else {
                    &mut system
                };
                for (value, field) in [(level.rms, &mut slot.0), (level.peak, &mut slot.1)] {
                    if let Some(value) = value.filter(|v| v.is_finite() && *v >= 0.) {
                        *field = Some(field.unwrap_or(0.).max(value));
                    }
                }
            }
        }
        self.mic
            .update(cx, |meter, cx| meter.sample(mic.0, mic.1, cx));
        self.system
            .update(cx, |meter, cx| meter.sample(system.0, system.1, cx));
    }
    pub(super) fn clear_ribbons(&mut self, cx: &mut Context<Self>) {
        for ribbon in self.audio_ui.ribbons.values() {
            ribbon.update(cx, |ribbon, cx| ribbon.clear(cx));
        }
    }
    fn audio_preferences(&self) -> AudioPreferences {
        let mut prefs: AudioPreferences = self
            .model
            .preferences
            .extra_preferences
            .get("audio_settings")
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default();
        if let Some(document) = &self.settings_document {
            prefs.advanced = document.modes.audio == frame_settings::model::SettingsMode::Advanced;
        }
        prefs
    }
    fn save_audio_preferences(&mut self, preferences: AudioPreferences, cx: &mut Context<Self>) {
        self.model.preferences.extra_preferences.insert(
            "audio_settings".into(),
            serde_json::to_value(preferences).expect("audio preferences are serializable"),
        );
        self.persist_appearance(cx);
    }
    fn audio_sources(&self) -> Vec<AudioSource> {
        let mut sources = self.model.snapshot.audio_sources.clone();
        for saved in self.audio_ui.known.values() {
            if !sources.iter().any(|s| s.id == saved.id) {
                let mut missing = saved.clone();
                missing.is_active = false;
                sources.push(missing);
            }
        }
        // A disconnected selected source remains visible and cannot silently become
        // another/default device. Route admission still uses the exact stable ID.
        if let Some(request) = &self.model.request {
            for selection in request
                .audio_routes
                .iter()
                .filter(|r| r.route != AudioRoute::Ignore)
            {
                if !sources.iter().any(|s| s.id == selection.source_id) {
                    let mut source = self
                        .audio_ui
                        .known
                        .get(&selection.source_id)
                        .cloned()
                        .unwrap_or_else(|| AudioSource {
                            controls: selection.controls,
                            capture_identity: None,
                            id: selection.source_id.clone(),
                            kind: AudioSourceKind::Microphone,
                            display_name: format!("Unavailable source · {}", selection.source_id.0),
                            process_id: None,
                            app_identifier: None,
                            is_active: false,
                            route: selection.route,
                            muted: selection.muted,
                        });
                    source.is_active = false;
                    source.route = selection.route;
                    source.muted = selection.muted;
                    sources.push(source);
                }
            }
        }
        sources
    }
    fn selected_audio_device(
        &self,
        kind: AudioSourceKind,
        remembered: Option<AudioSourceId>,
    ) -> Option<AudioSourceId> {
        let sources = self.audio_sources();
        // Current routes are authoritative over the remembered Easy selector.
        sources
            .iter()
            .find(|s| s.kind == kind && s.route != AudioRoute::Ignore)
            .map(|s| s.id.clone())
            .or(remembered)
            .or_else(|| {
                sources
                    .iter()
                    .find(|s| s.kind == kind && s.is_active)
                    .map(|s| s.id.clone())
            })
    }
    pub(super) fn remember_recorder_audio_devices(&mut self) {
        let prefs = self.audio_preferences();
        if self.audio_ui.recorder_microphone.is_none() {
            self.audio_ui.recorder_microphone =
                self.selected_audio_device(AudioSourceKind::Microphone, prefs.microphone);
        }
        if self.audio_ui.recorder_system.is_none() {
            self.audio_ui.recorder_system =
                self.selected_audio_device(AudioSourceKind::System, prefs.system);
        }
    }
    pub(super) fn recorder_audio_sources(&self) -> Vec<AudioSource> {
        let prefs = self.audio_preferences();
        let microphone = self.selected_audio_device(
            AudioSourceKind::Microphone,
            prefs
                .microphone
                .clone()
                .or(self.audio_ui.recorder_microphone.clone()),
        );
        let system = self.selected_audio_device(
            AudioSourceKind::System,
            prefs
                .system
                .clone()
                .or(self.audio_ui.recorder_system.clone()),
        );
        let mut selected: std::collections::BTreeSet<_> = prefs
            .recorder_sources
            .iter()
            .chain(prefs.routes.keys())
            .chain(self.audio_ui.order.iter())
            .cloned()
            .collect();
        selected.extend(microphone.iter().chain(system.iter()).cloned());
        selected.extend(prefs.microphone.iter().chain(prefs.system.iter()).cloned());
        if let Some(request) = &self.model.request {
            selected.extend(
                request
                    .audio_routes
                    .iter()
                    .filter(|r| r.route != AudioRoute::Ignore)
                    .map(|r| r.source_id.clone()),
            );
        }
        let mut sources = self.audio_sources();
        // Retain configured identities even while disconnected and switched off.
        // These rows do not enter the recording request or substitute a device.
        for id in &selected {
            if !sources.iter().any(|s| &s.id == id) {
                sources.push(AudioSource {
                    id: id.clone(),
                    kind: if system.as_ref() == Some(id)
                        || prefs.system.as_ref() == Some(id)
                        || id.0.starts_with("wasapi-loopback:")
                        || id.0.starts_with("wasapi-rest:")
                    {
                        AudioSourceKind::System
                    } else if id.0.starts_with("wasapi-session:")
                        || id.0.starts_with("app:")
                        || id.0.starts_with("pulse-app:")
                    {
                        AudioSourceKind::Application
                    } else {
                        AudioSourceKind::Microphone
                    },
                    display_name: format!("Unavailable source · {}", id.0),
                    controls: Default::default(),
                    capture_identity: None,
                    process_id: None,
                    app_identifier: None,
                    is_active: false,
                    route: AudioRoute::Ignore,
                    muted: false,
                });
            }
        }
        sources.retain(|s| {
            s.route != AudioRoute::Ignore
                || self.model.request.as_ref().is_some_and(|request| {
                    request
                        .audio_routes
                        .iter()
                        .any(|r| r.source_id == s.id && r.route != AudioRoute::Ignore)
                })
                || (selected.contains(&s.id) && !prefs.recorder_removed.contains(&s.id))
        });
        sources
    }
    pub(super) fn additional_audio_sources(&self) -> Vec<AudioSource> {
        let visible = self.recorder_audio_sources();
        self.audio_sources()
            .into_iter()
            .filter(|s| {
                s.is_active
                    && (s.kind != AudioSourceKind::Application
                        || self.model.snapshot.capabilities.per_app_audio)
                    && !visible.iter().any(|v| v.id == s.id)
            })
            .collect()
    }
    pub(super) fn add_recorder_audio_source(&mut self, id: AudioSourceId, cx: &mut Context<Self>) {
        if !self.panels_interactive()
            || !self.audio_sources().iter().any(|s| {
                s.id == id
                    && s.is_active
                    && (s.kind != AudioSourceKind::Application
                        || self.model.snapshot.capabilities.per_app_audio)
            })
        {
            return;
        }
        let mut prefs = self.audio_preferences();
        let removed = prefs.recorder_removed.contains(&id);
        prefs.recorder_removed.retain(|removed| removed != &id);
        if prefs.recorder_sources.contains(&id) {
            if removed {
                self.save_audio_preferences(prefs, cx);
            }
            return;
        }
        prefs.recorder_sources.push(id);
        self.save_audio_preferences(prefs, cx);
    }
    pub(super) fn can_remove_recorder_audio_source(&self, id: &AudioSourceId) -> bool {
        self.model.editable()
            && !self.model.level_testing
            && self.audio_ui.routes.is_empty()
            && self
                .recorder_audio_sources()
                .iter()
                .any(|s| &s.id == id && s.route == AudioRoute::Ignore)
            && self.model.request.as_ref().is_none_or(|request| {
                !request
                    .audio_routes
                    .iter()
                    .any(|r| &r.source_id == id && r.route != AudioRoute::Ignore)
            })
    }
    pub(super) fn remove_recorder_audio_source(
        &mut self,
        id: AudioSourceId,
        cx: &mut Context<Self>,
    ) {
        if !self.can_remove_recorder_audio_source(&id) {
            return;
        }
        let mut prefs = self.audio_preferences();
        prefs.recorder_sources.retain(|source| source != &id);
        if !prefs.recorder_removed.contains(&id) {
            prefs.recorder_removed.push(id.clone());
        }
        self.audio_ui.order.retain(|source| source != &id);
        self.audio_ui.ribbons.remove(&id);
        // This only edits Recorder membership. Disable is a separate explicit route action.
        self.save_audio_preferences(prefs, cx);
    }
    pub(super) fn validate_audio_recording(&self) -> Result<(), RecorderError> {
        routing::validate(
            &self.audio_sources(),
            self.model.snapshot.capabilities.per_app_audio,
        )
        .map(|_| ())
    }
    fn queue_audio_routes(
        &mut self,
        changes: Vec<(AudioSourceId, AudioRoute)>,
        cx: &mut Context<Self>,
    ) {
        if !self.model.editable() || self.model.level_testing || !self.audio_ui.routes.is_empty() {
            return;
        }
        // Lab sources have no capture identity. Keep UI edits local, before
        // hardware admission or engine commands, like the other preview routes.

        match routing::edits(
            &self.audio_sources(),
            &changes,
            self.model.snapshot.capabilities.per_app_audio,
        ) {
            Ok(changes) => {
                self.model.error = None;
                let sources = self.audio_sources();
                let mut prefs = self.audio_preferences();
                for (id, next) in &changes {
                    if let Some(source) = sources.iter().find(|s| &s.id == id) {
                        let remembered = if *next == AudioRoute::Ignore {
                            source.route
                        } else {
                            *next
                        };
                        if remembered != AudioRoute::Ignore {
                            prefs.routes.insert(id.clone(), remembered);
                        }
                    }
                }
                self.save_audio_preferences(prefs, cx);
                if sources.len() != self.model.snapshot.audio_sources.len() {
                    // Register retained unavailable identities so turning one off
                    // removes its saved route through the same engine command.
                    self.model
                        .apply_published(self.services.control.observe_sources(sources));
                }
                self.audio_ui.routes = changes.into();
                self.advance_audio_routes(cx);
            }
            Err(error) => self.error(error),
        }
        cx.notify();
    }
    pub(super) fn advance_audio_routes(&mut self, cx: &mut Context<Self>) {
        if self
            .audio_ui
            .processing
            .as_ref()
            .is_some_and(|(id, controls)| {
                self.model
                    .snapshot
                    .audio_sources
                    .iter()
                    .any(|s| &s.id == id && &s.controls == controls)
            })
        {
            self.audio_ui.processing = None;
            self.model.command_pending = false;
        }
        if self.model.error.is_some() || self.model.snapshot.state != RecorderState::Idle {
            self.audio_ui.routes.clear();
            self.audio_ui.processing = None;
            return;
        }
        if self.model.pending_route.is_some() || self.model.command_pending {
            return;
        }
        if let Some((id, next)) = self.audio_ui.routes.pop_front() {
            match self.services.control.update_route(id.clone(), next) {
                Ok(()) => {
                    self.model.pending_route = Some((id, next));
                    self.model.command_pending = true;
                }
                Err(error) => {
                    self.audio_ui.routes.clear();
                    self.error(error);
                }
            }
            cx.notify();
        }
    }
    pub(super) fn audio_settings_event(
        &mut self,
        event: AudioSettingsEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut prefs = self.audio_preferences();
        match event {
            AudioSettingsEvent::Output(output) => {
                self.audio_ui.bitrate_draft = None;
                self.apply_audio_output(output, cx);
            }
            AudioSettingsEvent::BitrateDraft(message) => {
                self.audio_ui.bitrate_draft = message;
                if self.audio_ui.bitrate_draft.is_none() {
                    self.flush_save(cx);
                }
            }
            AudioSettingsEvent::Source(id) => {
                if self.audio_sources().iter().any(|s| s.id == id) {
                    self.audio_ui.selected_source = Some(id);
                }
            }
            AudioSettingsEvent::TrackMode(mode)
                if mode != AudioRoute::Ignore
                    && self.model.editable()
                    && !self.model.level_testing =>
            {
                let changes = self
                    .audio_sources()
                    .iter()
                    .filter(|s| s.route != AudioRoute::Ignore)
                    .map(|s| (s.id.clone(), mode))
                    .collect();
                self.queue_audio_routes(changes, cx);
                if self.model.error.is_none() {
                    prefs = self.audio_preferences();
                    prefs.track_mode = Some(mode);
                    prefs.separate = routing::individual(mode);
                    for route in prefs.routes.values_mut() {
                        *route = mode;
                    }
                    self.save_audio_preferences(prefs, cx);
                }
            }
            AudioSettingsEvent::Mode(mode) => {
                self.audio_ui.bitrate_draft = None;
                if self.preferences_loaded {
                    if self.settings_document.is_none() {
                        self.install_settings_document(None);
                    }
                    if let Some(document) = &mut self.settings_document {
                        document.modes.audio = if mode == AudioSettingsMode::Advanced {
                            frame_settings::model::SettingsMode::Advanced
                        } else {
                            frame_settings::model::SettingsMode::Easy
                        };
                    }
                    prefs.advanced = mode == AudioSettingsMode::Advanced;
                    self.save_audio_preferences(prefs, cx);
                    self.persist_mode(cx);
                }
            }
            AudioSettingsEvent::Controls(id, controls)
                if self.model.editable() && !self.model.level_testing =>
            {
                match self
                    .services
                    .control
                    .update_audio_controls(id.clone(), controls)
                {
                    Ok(()) => {
                        self.audio_ui.processing = Some((id, controls));
                        self.model.command_pending = true;
                    }
                    Err(e) => self.error(e),
                }
            }
            AudioSettingsEvent::Route(id, next) => self.route(id, next, cx),
            AudioSettingsEvent::Mute(id, muted) => {
                self.dispatch(Action::MuteAudio(id, muted), window, cx)
            }
            AudioSettingsEvent::Separate(enabled)
                if self.model.editable() && !self.model.level_testing =>
            {
                let changes = routing::separate_edits(&self.audio_sources(), enabled);
                self.queue_audio_routes(changes, cx);
                if self.model.error.is_none() {
                    prefs.separate = enabled;
                    prefs.routes = self.audio_preferences().routes;
                    self.save_audio_preferences(prefs, cx);
                }
            }
            AudioSettingsEvent::IncludeInMix => {
                let changes = self
                    .audio_sources()
                    .iter()
                    .filter(|s| s.route != AudioRoute::Ignore)
                    .map(|s| {
                        (
                            s.id.clone(),
                            routing::route(true, routing::individual(s.route)),
                        )
                    })
                    .collect();
                self.queue_audio_routes(changes, cx);
            }
            AudioSettingsEvent::Device(kind, id)
                if self.model.editable() && !self.model.level_testing =>
            {
                let sources = self.audio_sources();
                if !sources
                    .iter()
                    .any(|s| s.id == id && s.kind == kind && s.is_active)
                {
                    return;
                }
                let group: Vec<_> = sources.iter().filter(|s| s.kind == kind).collect();
                let active = group.iter().any(|s| s.route != AudioRoute::Ignore);
                // Replacing a device preserves the group's actual destination,
                // including Separate-only chosen in Advanced.
                let destination = group
                    .iter()
                    .find(|s| s.route != AudioRoute::Ignore)
                    .map(|s| s.route)
                    .or(prefs.track_mode)
                    .unwrap_or_else(|| routing::route(true, prefs.separate));
                if active {
                    // Off first, then enable the replacement. Never open both devices
                    // as a side effect of choosing a single Easy device.
                    let mut changes: Vec<_> = group
                        .iter()
                        .filter(|s| s.id != id && s.route != AudioRoute::Ignore)
                        .map(|s| (s.id.clone(), AudioRoute::Ignore))
                        .collect();
                    changes.push((id.clone(), destination));
                    self.queue_audio_routes(changes, cx);
                    if self.model.error.is_some() {
                        return;
                    }
                }
                match kind {
                    AudioSourceKind::Microphone => prefs.microphone = Some(id),
                    AudioSourceKind::System => prefs.system = Some(id),
                    _ => return,
                }
                prefs.routes = self.audio_preferences().routes;
                self.save_audio_preferences(prefs, cx);
            }
            AudioSettingsEvent::Enabled(kind, enabled) => {
                let remembered = if kind == AudioSourceKind::Microphone {
                    prefs.microphone
                } else {
                    prefs.system
                };
                let selected = self.selected_audio_device(kind, remembered);
                let sources = self.audio_sources();
                let mut changes: Vec<_> = sources
                    .iter()
                    .filter(|s| s.kind == kind && s.route != AudioRoute::Ignore)
                    .map(|s| (s.id.clone(), AudioRoute::Ignore))
                    .collect();
                if enabled && let Some(id) = selected {
                    changes.retain(|(other, _)| *other != id);
                    let next = prefs
                        .routes
                        .get(&id)
                        .copied()
                        .filter(|r| *r != AudioRoute::Ignore)
                        .unwrap_or_else(|| {
                            prefs
                                .track_mode
                                .unwrap_or_else(|| routing::route(true, prefs.separate))
                        });
                    changes.push((id, next));
                }
                self.queue_audio_routes(changes, cx);
            }
            AudioSettingsEvent::Refresh if self.model.editable() && !self.model.level_testing => {
                self.refresh_audio_devices(cx)
            }
            AudioSettingsEvent::Test => self.test_levels(window, cx),
            _ => {}
        }
        cx.notify();
    }
    fn refresh_audio_devices(&mut self, cx: &mut Context<Self>) {
        self.audio_ui.remember(&self.model.snapshot.audio_sources);
        let previous = self.audio_sources();
        let control = self.services.control.clone();
        self.model.connecting = true;
        let task = cx.background_executor().spawn(async move {
            frame_runtime::audio_discovery::discover().map(|snapshot| {
                let mut sources = snapshot.sources;
                for mut missing in previous
                    .into_iter()
                    .filter(|s| s.route != AudioRoute::Ignore)
                {
                    if !sources.iter().any(|s| s.id == missing.id) {
                        missing.is_active = false;
                        sources.push(missing);
                    }
                }
                (
                    control.observe_sources(sources),
                    snapshot.per_app_audio,
                    snapshot.audio_levels,
                )
            })
        });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                match result {
                    Ok((published, per_app, audio_levels)) => {
                        this.model.apply_published(published);
                        this.model.snapshot.capabilities.per_app_audio = per_app;
                        this.model.snapshot.capabilities.audio_levels = audio_levels;
                        this.model.sync_routes();
                        this.model.error = None;
                    }
                    Err(error) => this.error(error),
                }
                this.model.connecting = false;
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn audio_sheet(&self, cx: &mut Context<Self>) -> AnyElement {
        let prefs = self.audio_preferences();
        let sources = self.audio_sources();
        let plan = frame_audio::plan_tracks(&routing::selections(&sources));
        let track_summary = match &plan {
            Ok(plan) if plan.is_empty() => {
                "Audio is off. Recordings will contain video only.".into()
            }
            Ok(plan) => format!(
                "{} audio tracks · {} main mix · {} individual",
                plan.len(),
                usize::from(
                    plan.iter()
                        .any(|t| t.kind == frame_audio::TrackKind::MainMix)
                ),
                plan.iter()
                    .filter(|t| matches!(t.kind, frame_audio::TrackKind::Clean(_)))
                    .count()
            ),
            Err(error) => error.message.clone(),
        };
        let mut issues =
            routing::validate(&sources, self.model.snapshot.capabilities.per_app_audio)
                .err()
                .map(|error| vec![error.message])
                .unwrap_or_default();

        let unsupported_output = self
            .settings_document
            .as_ref()
            .and_then(|d| d.configuration().ok())
            .is_some_and(|c| c.audio.validate().is_err());
        if unsupported_output {
            issues.push("Saved audio output is unavailable. Choose Opus at 48 kHz, 6–510 kb/s per track, and mono/stereo or preserved channels.".into());
        }
        let entries = sources
            .iter()
            .map(|source| {
                let individual_track = plan
                    .as_ref()
                    .ok()
                    .and_then(|plan| {
                        plan.iter().position(|track| {
                            track.kind == frame_audio::TrackKind::Clean(source.id.clone())
                        })
                    })
                    .map(|index| format!("Track {}", index + 1));
                AudioSettingsSource {
                    source: source.clone(),
                    can_mute: self.can_mute(&source.id),
                    connection: self
                        .model
                        .snapshot
                        .warnings
                        .iter()
                        .rev()
                        .find(|warning| {
                            matches!(
                                warning.code.as_str(),
                                "audioSourceReconnecting"
                                    | "audioSourceEnded"
                                    | "audioSourceRestored"
                            ) && warning.message.contains(&source.id.0)
                        })
                        .map(|warning| {
                            match warning.code.as_str() {
                                "audioSourceReconnecting" => "Reconnecting · same device",
                                "audioSourceEnded" => "Capture ended · silent gaps",
                                _ => "Capture resumed",
                            }
                            .into()
                        })
                        .unwrap_or_else(|| {
                            if source.is_active {
                                "Available".into()
                            } else {
                                "Unavailable · exact device selection retained".into()
                            }
                        }),
                    individual_track,
                }
            })
            .collect();
        let reconnect = match frame_audio::AudioContinuityPolicy::from_environment() {
            Ok(policy) if policy.reconnect_source => "Disconnect: silent gaps while the same source is retried for up to 5 seconds. Track identities stay fixed; no fallback device.".into(),
            Ok(_) => "Disconnect: source loss remains silence in its existing track. Automatic reconnect is off. Reconnect the device and refresh before the next recording.".into(),
            Err(error) => error.message,
        };
        let props = AudioSettingsProps {
            output_revision: self.audio_ui.output_revision,
            track_mode: {
                let active: Vec<_> = sources
                    .iter()
                    .filter(|s| s.route != AudioRoute::Ignore)
                    .collect();
                if active.is_empty() {
                    Some(
                        prefs
                            .track_mode
                            .unwrap_or_else(|| routing::route(true, prefs.separate)),
                    )
                } else {
                    active
                        .iter()
                        .all(|s| s.route == active[0].route)
                        .then_some(active[0].route)
                }
            },
            selected_source: self.audio_ui.selected_source.clone(),
            output: self
                .settings_document
                .as_ref()
                .and_then(|d| d.configuration().ok())
                .map(|c| c.audio)
                .or_else(|| self.model.request.as_ref().map(|r| r.audio_encoding))
                .unwrap_or_default(),
            mode: if prefs.advanced {
                AudioSettingsMode::Advanced
            } else {
                AudioSettingsMode::Easy
            },
            microphone: self.selected_audio_device(AudioSourceKind::Microphone, prefs.microphone),
            system: self.selected_audio_device(AudioSourceKind::System, prefs.system),
            sources: entries,
            editable: self.model.editable()
                && !self.model.level_testing
                && !self.model.settings_blocked,
            per_app: self.model.snapshot.capabilities.per_app_audio,
            can_test: self.model.editable()
                && self.model.snapshot.capabilities.audio_levels
                && (self.model.level_testing
                    || (sources
                        .iter()
                        .any(|s| s.is_active && s.route != AudioRoute::Ignore)
                        && issues.is_empty())),
            testing: self.model.level_testing,
            track_summary,
            issues,
            reconnect,
        };
        let view = cx.weak_entity();
        let panel = AudioSettingsPanel::new(props, move |event, window, cx| {
            let _ = view.update(cx, |this, cx| {
                this.dispatch(Action::AudioSettings(event), window, cx)
            });
        });
        let mut content = crate::ui::primitives::Stack::vertical().child(panel);
        if unsupported_output {
            content = content.child(self.button(
                Action::UseSupportedAudio,
                "Use supported Opus output",
                None,
                false,
                self.model.editable(),
                cx,
            ));
        }
        content.into_any_element()
    }
    pub(super) fn route(&mut self, id: AudioSourceId, route: AudioRoute, cx: &mut Context<Self>) {
        self.queue_audio_routes(vec![(id, route)], cx);
    }
    pub(super) fn enabled_audio_route(&self, id: &AudioSourceId) -> AudioRoute {
        self.audio_preferences()
            .routes
            .get(id)
            .copied()
            .filter(|r| *r != AudioRoute::Ignore)
            .unwrap_or(AudioRoute::MainMix)
    }
    pub(super) fn test_levels(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.model.snapshot.capabilities.audio_levels || !self.model.editable() {
            return;
        }
        let next = !self.model.level_testing;
        if next && let Err(error) = self.validate_audio_recording() {
            self.error(error);
            cx.notify();
            return;
        }
        let services = self.services.clone();
        let ids = self
            .model
            .snapshot
            .audio_sources
            .iter()
            .filter(|s| s.route != AudioRoute::Ignore)
            .map(|s| s.id.clone())
            .collect::<Vec<_>>();
        let task = cx.background_executor().spawn(async move {
            #[cfg(target_os = "windows")]
            {
                if next {
                    services.control.start_audio_level_test(ids)
                } else {
                    services.control.stop_audio_level_test()
                }
            }
            #[cfg(not(target_os = "windows"))]
            {
                let _ = (services, ids);
                Err(RecorderError::unsupported("Audio test"))
            }
        });
        cx.spawn_in(window, async move |view, cx| {
            let result = task.await;
            let _ = view.update_in(cx, |this, window, cx| {
                match result {
                    Ok(()) => this.model.level_testing = next,
                    Err(e) => this.error(e),
                }
                this.update_meter_task(window, cx);
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn update_meter_task(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.meter_task = None;
        if !self.model.level_testing
            && !matches!(
                self.model.snapshot.state,
                RecorderState::Recording | RecorderState::Paused
            )
        {
            self.mic.update(cx, |m, cx| m.sample(None, None, cx));
            self.system.update(cx, |m, cx| m.sample(None, None, cx));
            self.clear_ribbons(cx);
            return;
        }
        let services = self.services.clone();
        self.meter_task = Some(cx.spawn_in(window, async move |view, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(
                        crate::ui::theme::motion::METER_SAMPLE_MS,
                    ))
                    .await;
                let visible = view
                    .update_in(cx, |this, window, cx| {
                        let visible = !this.model.closing
                            && window.is_visible()
                            && (this.model.sheet.is_none()
                                || this.model.sheet == Some(Sheet::Audio)
                                || (this.model.sheet == Some(Sheet::Settings)
                                    && this.settings_category == 2));
                        if !visible {
                            this.clear_ribbons(cx);
                        }
                        visible
                    })
                    .unwrap_or(false);
                if !visible {
                    cx.background_executor()
                        .timer(Duration::from_millis(210))
                        .await;
                    continue;
                }
                #[cfg(target_os = "windows")]
                let levels = services.control.try_audio_levels();
                #[cfg(not(target_os = "windows"))]
                let levels = Some(AudioLevelsSnapshot {
                    session_id: None,
                    sources: vec![],
                });
                if view
                    .update(cx, |this, cx| {
                        if let Some(levels) = &levels {
                            this.feed_meter_snapshot(levels, cx);
                        } else {
                            for ribbon in this.audio_ui.ribbons.values() {
                                ribbon.update(cx, |ribbon, cx| ribbon.expire(cx));
                            }
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }
}
