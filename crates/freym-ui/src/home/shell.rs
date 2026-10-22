use super::*;
use gpui::div;

use crate::ui::{components::*, primitives::*, theme::spacing::Spacing};

impl Home {
    pub(super) fn handler(
        &self,
        action: Action,
        cx: &Context<Self>,
    ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static {
        let view = cx.weak_entity();
        let owner = self.owner_window;
        move |_, window, cx| {
            // Recording tasks always belong to the Recorder, even when the
            // originating compact menu is closed before acknowledgement arrives.
            if matches!(action, Action::Record | Action::Pause) && window.window_handle() != owner {
                let _ = owner.update(cx, |_, window, cx| {
                    let _ = view.update(cx, |this, cx| this.dispatch(action.clone(), window, cx));
                });
            } else {
                let _ = view.update(cx, |this, cx| this.dispatch(action.clone(), window, cx));
            }
        }
    }

    pub(super) fn button(
        &self,
        action: Action,
        label: impl Into<String>,
        glyph: Option<Icons>,
        selected: bool,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut button = Button::new(format!("{action:?}"), label.into())
            .selected(selected)
            .disabled(!enabled)
            .on_click(self.handler(action, cx));

        if let Some(glyph) = glyph {
            button = button.icon(glyph);
        }

        button.into_any_element()
    }

    pub(super) fn recording_button(&self, cx: &mut Context<Self>) -> RecordButton {
        let state = match self.model.snapshot.state {
            RecorderState::Idle => RecordButtonState::Ready,

            RecorderState::Preparing => RecordButtonState::Countdown,

            RecorderState::Recording => RecordButtonState::Recording,

            RecorderState::Paused => RecordButtonState::Paused,

            RecorderState::Finalizing => RecordButtonState::Stopping,

            RecorderState::Failed(_) => RecordButtonState::Failed,
        };

        let enabled = if state == RecordButtonState::Ready {
            self.model.can_start()
        } else {
            !self.model.command_pending
                && !self.model.closing
                && state != RecordButtonState::Stopping
        };

        RecordButton::new(state)
            .label(self.model.record_label())
            .disabled(!enabled)
            .on_click(self.handler(Action::Record, cx))
    }

    pub(super) fn home_audio_panel(&self, cx: &mut Context<Self>) -> RecorderAudioPanel {
        let editable = self.model.editable();
        let meters = Stack::horizontal()
            .align(Alignment::Between)
            .child(
                Stack::vertical()
                    .gap(Spacing::Xxs)
                    .child(Text::new("Microphone").variant(TextVariant::Caption))
                    .child(self.mic.clone()),
            )
            .child(
                Stack::vertical()
                    .gap(Spacing::Xxs)
                    .child(Text::new("Output").variant(TextVariant::Caption))
                    .child(self.system.clone()),
            );
        let mut panel = RecorderAudioPanel::new(
            IconButton::new("audio-settings", Icons::Settings, "Audio routing settings")
                .on_click(self.handler(Action::Audio, cx)),
            Button::new(
                "test-home-audio",
                if self.model.level_testing {
                    "Stop test"
                } else {
                    "Test audio"
                },
            )
            .size(ButtonSize::Compact)
            .variant(ButtonVariant::Secondary)
            .icon(Icons::Audio)
            .disabled(!editable || !self.model.snapshot.capabilities.audio_levels)
            .on_click(self.handler(Action::TestLevels, cx)),
            meters,
        )
        .expanded(self.model.preferences.recorder_sections.audio)
        .on_click(self.handler(Action::ToggleSection(RecorderSection::Audio), cx));
        if self.model.snapshot.audio_sources.is_empty() {
            panel = panel.child(Text::new("No audio sources").variant(TextVariant::Muted));
        }
        let first_mic = self
            .model
            .snapshot
            .audio_sources
            .iter()
            .find(|s| s.kind == AudioSourceKind::Microphone)
            .map(|s| &s.id);
        for source in &self.model.snapshot.audio_sources {
            let kind = match source.kind {
                AudioSourceKind::Microphone => AudioKind::Microphone,
                AudioSourceKind::System => AudioKind::System,
                AudioSourceKind::Application => AudioKind::Application,
            };
            let id = if first_mic == Some(&source.id) {
                "mic-toggle".into()
            } else {
                format!("home-audio-{}", source.id.0)
            };
            let (route_label, next_route) = match source.route {
                AudioRoute::Ignore => ("Off", AudioRoute::MainMix),
                AudioRoute::MainMix => ("Mixed", AudioRoute::SeparateOnly),
                AudioRoute::SeparateOnly => ("Separate", AudioRoute::MainMixAndSeparate),
                AudioRoute::MainMixAndSeparate => ("Both", AudioRoute::MainMix),
            };
            panel = panel.child(
                AudioSourceRow::new(id, source.display_name.clone(), kind)
                    .enabled(source.route != AudioRoute::Ignore)
                    .connected(source.is_active)
                    .disabled(!editable)
                    .on_click(self.handler(Action::ToggleAudio(source.id.clone()), cx))
                    .inline()
                    .expanded(self.model.preferences.recorder_sections.audio)
                    .controls(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                Button::new(format!("audio-routing-{}", source.id.0), route_label)
                                    .size(ButtonSize::Compact)
                                    .disabled(!editable)
                                    .on_click(self.handler(
                                        Action::SelectRoute(source.id.clone(), next_route),
                                        cx,
                                    )),
                            )
                            .child(
                                IconButton::new(
                                    format!("audio-mute-{}", source.id.0),
                                    Icons::Audio,
                                    if source.muted { "Unmute" } else { "Mute" },
                                )
                                .selected(source.muted)
                                .disabled(!self.can_mute(&source.id))
                                .on_click(self.handler(
                                    Action::MuteAudio(source.id.clone(), !source.muted),
                                    cx,
                                )),
                            ),
                    ),
            );
        }
        panel
    }

    pub(super) fn inline_video(&self, cx: &mut Context<Self>) -> AnyElement {
        let fps = self
            .model
            .request
            .as_ref()
            .map(|r| match r.fps_mode {
                FpsMode::AutoMatchSource => "Auto FPS".into(),
                FpsMode::Fixed(fps) => format!("{fps} FPS"),
            })
            .unwrap_or_else(|| "Auto FPS".into());
        div()
            .w_full()
            .flex()
            .flex_wrap()
            .items_center()
            .flex_shrink_0()
            .gap(Spacing::Xs.px())
            .child(
                Button::new("inline-source-picker", self.model.source_label())
                    .size(ButtonSize::Compact)
                    .icon(Icons::Screen)
                    .disabled(!self.model.editable())
                    .on_click(self.handler(Action::OpenSourcePicker, cx)),
            )
            .child(
                Button::new("inline-fps", fps)
                    .size(ButtonSize::Compact)
                    .disabled(!self.model.editable() || !self.model.snapshot.capabilities.fixed_fps)
                    .on_click(self.handler(Action::Fps, cx)),
            )
            .child(
                Button::new("inline-video-settings", "Video settings…")
                    .size(ButtonSize::Compact)
                    .on_click(self.handler(Action::QualityOptions, cx)),
            )
            .child(self.toggle(
                Action::Cursor,
                "Cursor",
                self.model.request.as_ref().is_some_and(|r| r.show_cursor),
                self.model.editable(),
                cx,
            ))
            .into_any_element()
    }

    pub(super) fn inline_camera(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut controls = div()
            .w_full()
            .flex()
            .flex_wrap()
            .items_center()
            .flex_shrink_0()
            .gap(Spacing::Xs.px())
            .child(
                Button::new("inline-find-cameras", "Find cameras")
                    .size(ButtonSize::Compact)
                    .icon(Icons::Refresh)
                    .disabled(!self.model.editable())
                    .on_click(self.handler(Action::FindCameras, cx)),
            )
            .child(
                Button::new("inline-camera-settings", "Configure…")
                    .size(ButtonSize::Compact)
                    .disabled(!self.model.editable())
                    .on_click(self.handler(Action::Source(SourceMode::Camera), cx)),
            );
        if self
            .model
            .request
            .as_ref()
            .is_some_and(|r| r.camera.is_some())
        {
            controls = controls.child(
                Button::new("inline-camera-off", "Off")
                    .size(ButtonSize::Compact)
                    .disabled(!self.model.editable())
                    .on_click(self.handler(Action::RemoveCamera, cx)),
            );
        }
        controls.into_any_element()
    }

    pub(super) fn inline_quality(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut controls = div()
            .w_full()
            .flex()
            .flex_wrap()
            .items_center()
            .flex_shrink_0()
            .gap(Spacing::Xs.px())
            .child(
                Button::new("inline-quality-auto", "Auto")
                    .size(ButtonSize::Compact)
                    .selected(self.model.automatic_quality())
                    .disabled(!self.model.editable())
                    .on_click(self.handler(Action::AutomaticQuality, cx)),
            );
        for quality in [Quality::Standard, Quality::High, Quality::Maximum] {
            controls = controls.child(
                Button::new(
                    format!("inline-quality-{quality:?}"),
                    format!("{quality:?}"),
                )
                .size(ButtonSize::Compact)
                .selected(
                    self.model
                        .request
                        .as_ref()
                        .is_some_and(|r| r.quality == quality),
                )
                .disabled(
                    !self.model.editable()
                        || self.model.required_quality().is_some_and(|q| q != quality),
                )
                .on_click(self.handler(Action::SelectQuality(quality), cx)),
            );
        }
        controls
            .child(
                Button::new("inline-quality-settings", "Details…")
                    .size(ButtonSize::Compact)
                    .on_click(self.handler(Action::QualityOptions, cx)),
            )
            .into_any_element()
    }

    pub(super) fn setting(&self, label: &'static str, control: AnyElement) -> AnyElement {
        SettingRow::new(label, control).into_any_element()
    }

    pub(super) fn output_row(&self, cx: &mut Context<Self>) -> OutputFolderRow {
        let label = self
            .model
            .request
            .as_ref()
            .and_then(|r| std::path::Path::new(&r.output.directory).file_name())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Output".into());

        OutputFolderRow::new("Output", label)
            .disabled(!self.model.editable())
            .on_click(self.handler(Action::Output, cx))
    }

    pub(super) fn toggle(
        &self,
        action: Action,
        label: &'static str,
        checked: bool,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        Toggle::new(format!("{action:?}"), label, checked)
            .disabled(!enabled)
            .on_click(self.handler(action, cx))
            .into_any_element()
    }

    pub(super) fn controls(&self, cx: &mut Context<Self>) -> RecordControls {
        let mut controls = RecordControls::new(self.recording_button(cx)).accessory(
            div()
                .flex()
                .items_center()
                .gap(px(4.))
                .child(
                    Button::new(
                        "home-countdown",
                        if self.model.preferences.countdown == 0 {
                            "Countdown".into()
                        } else {
                            format!("Countdown: {} s", self.model.preferences.countdown)
                        },
                    )
                    .size(ButtonSize::Dock)
                    .icon(Icons::Timer)
                    .disabled(!self.model.editable())
                    .on_click(self.handler(Action::Countdown, cx)),
                )
                .child(
                    Button::new("Output", "Recording folder")
                        .size(ButtonSize::Dock)
                        .icon(Icons::Folder)
                        .disabled(!self.model.editable())
                        .on_click(self.handler(Action::Output, cx)),
                )
                .child(
                    Button::new("home-settings", "Settings")
                        .size(ButtonSize::Dock)
                        .icon(Icons::Settings)
                        .on_click(self.handler(Action::Settings, cx)),
                )
                .child(
                    Button::new("Studio", "Studio")
                        .size(ButtonSize::Dock)
                        .icon(Icons::Window)
                        .disabled(!self.model.editable())
                        .on_click(self.handler(Action::Studio, cx)),
                ),
        );
        if matches!(
            self.model.snapshot.state,
            RecorderState::Recording | RecorderState::Paused
        ) {
            let paused = self.model.snapshot.state == RecorderState::Paused;
            let elapsed = format_elapsed(
                self.model
                    .snapshot
                    .metrics
                    .as_ref()
                    .map(|m| m.elapsed_ms)
                    .unwrap_or(0.),
            );
            controls = controls.session(
                Button::new(
                    "Pause",
                    format!("{} · {elapsed}", if paused { "Resume" } else { "Pause" }),
                )
                .size(ButtonSize::Record)
                .variant(ButtonVariant::Secondary)
                .icon(if paused { Icons::Play } else { Icons::Pause })
                .disabled(!self.model.snapshot.capabilities.can_pause || self.model.command_pending)
                .on_click(self.handler(Action::Pause, cx)),
                elapsed,
            );
        }
        controls
    }
}
