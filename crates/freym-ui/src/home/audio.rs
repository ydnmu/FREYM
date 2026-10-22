use super::*;
#[cfg(test)]
#[path = "audio/tests.rs"]
mod tests;
use crate::{audio_adapter as routing, ui::components::*};
use std::collections::{BTreeMap, VecDeque};

#[derive(Default)]
pub(super) struct AudioUi {
    pub(super) routes: VecDeque<(AudioSourceId, AudioRoute)>,
    known: BTreeMap<AudioSourceId, AudioSource>,
    processing: Option<(AudioSourceId, AudioSourceControls)>,
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
        match routing::edits(
            &self.audio_sources(),
            &changes,
            self.model.snapshot.capabilities.per_app_audio,
        ) {
            Ok(changes) => {
                self.model.error = None;
                let sources = self.audio_sources();
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
            AudioSettingsEvent::Mode(mode) => {
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
                let separate = group.iter().any(|s| routing::individual(s.route)) || prefs.separate;
                if active {
                    // Off first, then enable the replacement. Never open both devices
                    // as a side effect of choosing a single Easy device.
                    let mut changes: Vec<_> = group
                        .iter()
                        .filter(|s| s.id != id && s.route != AudioRoute::Ignore)
                        .map(|s| (s.id.clone(), AudioRoute::Ignore))
                        .collect();
                    changes.push((id.clone(), routing::route(true, separate)));
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
                    changes.push((id, routing::route(true, prefs.separate)));
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
            .is_some_and(|c| c.audio != frame_settings::model::AudioSettings::default());
        if unsupported_output {
            issues.push("Saved audio output is unavailable. The engine supports Opus at 48 kHz and 128 kb/s per track.".into());
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
            mode: if prefs.advanced {
                AudioSettingsMode::Advanced
            } else {
                AudioSettingsMode::Easy
            },
            microphone: self.selected_audio_device(AudioSourceKind::Microphone, prefs.microphone),
            system: self.selected_audio_device(AudioSourceKind::System, prefs.system),
            sources: entries,
            editable: self.model.editable() && !self.model.level_testing,
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
                #[cfg(target_os = "windows")]
                let levels = services.control.audio_levels();
                #[cfg(not(target_os = "windows"))]
                let levels = AudioLevelsSnapshot {
                    session_id: None,
                    sources: vec![],
                };
                if view
                    .update(cx, |this, cx| {
                        let mut mic = (None, None);
                        let mut system = (None, None);
                        for l in &levels.sources {
                            if let Some(s) = this
                                .model
                                .snapshot
                                .audio_sources
                                .iter()
                                .find(|s| s.id == l.source_id)
                            {
                                let slot = if s.kind == AudioSourceKind::Microphone {
                                    &mut mic
                                } else {
                                    &mut system
                                };
                                slot.0 = Some(slot.0.unwrap_or(0f32).max(l.rms.unwrap_or(0.)));
                                slot.1 = Some(slot.1.unwrap_or(0f32).max(l.peak.unwrap_or(0.)));
                            }
                        }
                        this.mic.update(cx, |m, cx| m.sample(mic.0, mic.1, cx));
                        this.system
                            .update(cx, |m, cx| m.sample(system.0, system.1, cx));
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }
}
