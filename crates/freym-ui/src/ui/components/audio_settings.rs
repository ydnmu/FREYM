//! Native audio settings composition. Controls emit intentions to the audio owner.
use super::{SegmentedControl, SettingRow, Toggle};
use crate::ui::{
    primitives::*,
    theme::{colors::Color, spacing::Spacing},
};
use frame_contracts::*;
use gpui::{
    AnyElement, App, ElementId, IntoElement, RenderOnce, SharedString, Window, div, prelude::*,
};
use std::rc::Rc;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AudioSettingsMode {
    #[default]
    Easy,
    Advanced,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioSettingsEvent {
    Mode(AudioSettingsMode),
    Route(AudioSourceId, AudioRoute),
    Mute(AudioSourceId, bool),
    Controls(AudioSourceId, AudioSourceControls),
    Device(AudioSourceKind, AudioSourceId),
    Enabled(AudioSourceKind, bool),
    Separate(bool),
    IncludeInMix,
    Refresh,
    Test,
}

#[derive(Clone)]
pub struct AudioSettingsSource {
    pub source: AudioSource,
    pub can_mute: bool,
    pub connection: String,
    pub individual_track: Option<String>,
}

pub struct AudioSettingsProps {
    pub mode: AudioSettingsMode,
    pub sources: Vec<AudioSettingsSource>,
    pub microphone: Option<AudioSourceId>,
    pub system: Option<AudioSourceId>,
    pub editable: bool,
    pub per_app: bool,
    pub can_test: bool,
    pub testing: bool,
    pub track_summary: String,
    pub issues: Vec<String>,
    pub reconnect: String,
}

type Listener = Rc<dyn Fn(AudioSettingsEvent, &mut Window, &mut App)>;

#[derive(IntoElement)]
pub struct AudioSettingsPanel {
    props: AudioSettingsProps,
    listener: Listener,
}
impl AudioSettingsPanel {
    pub fn new(
        props: AudioSettingsProps,
        listener: impl Fn(AudioSettingsEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            props,
            listener: Rc::new(listener),
        }
    }
    fn button(
        &self,
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        event: AudioSettingsEvent,
        disabled: bool,
    ) -> Button {
        let listener = self.listener.clone();
        Button::new(id, label)
            .size(ButtonSize::Compact)
            .disabled(disabled)
            .on_click(move |_, window, cx| listener(event.clone(), window, cx))
    }
    fn toggle(
        &self,
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        checked: bool,
        event: AudioSettingsEvent,
        disabled: bool,
    ) -> Toggle {
        let listener = self.listener.clone();
        Toggle::new(id, label, checked)
            .disabled(disabled)
            .on_click(move |_, window, cx| listener(event.clone(), window, cx))
    }
    fn easy_device(
        &self,
        kind: AudioSourceKind,
        label: &'static str,
        selection: &Option<AudioSourceId>,
    ) -> AnyElement {
        let sources: Vec<_> = self
            .props
            .sources
            .iter()
            .filter(|s| s.source.kind == kind)
            .collect();
        let enabled = sources.iter().any(|s| s.source.route != AudioRoute::Ignore);
        let prefix = if kind == AudioSourceKind::Microphone {
            "microphone"
        } else {
            "system"
        };
        let selected = sources
            .iter()
            .find(|s| Some(&s.source.id) == selection.as_ref());
        let mut group = Stack::vertical().gap(Spacing::Xs).child(SettingRow::new(
            label,
            self.toggle(
                format!("audio-easy-{prefix}"),
                format!("{label} on or off"),
                enabled,
                AudioSettingsEvent::Enabled(kind, !enabled),
                !self.props.editable
                    || sources.is_empty()
                    || (!enabled && selected.is_none_or(|s| !s.source.is_active)),
            ),
        ));
        if sources.is_empty() {
            group = group.child(
                Text::new(if kind == AudioSourceKind::System {
                    "System audio capture is unavailable on this backend."
                } else {
                    "No microphone is available. Connect one and refresh devices."
                })
                .variant(TextVariant::Muted),
            );
        } else {
            if selection.is_some() && selected.is_none() {
                group = group.child(Text::new("The selected device is unavailable. Choose a device explicitly before enabling audio.").variant(TextVariant::Muted).color(Color::Warning));
            }
            for entry in &sources {
                let s = &entry.source;
                group = group.child(
                    self.button(
                        format!("audio-device-{}", s.id.0),
                        s.display_name.clone(),
                        AudioSettingsEvent::Device(kind, s.id.clone()),
                        !self.props.editable || !s.is_active,
                    )
                    .icon(if kind == AudioSourceKind::Microphone {
                        Icons::Mic
                    } else {
                        Icons::Audio
                    })
                    .selected(selection.as_ref() == Some(&s.id)),
                );
            }
            if sources
                .iter()
                .filter(|s| s.source.route != AudioRoute::Ignore)
                .count()
                > 1
            {
                group = group.child(Text::new("Multiple devices are enabled. Advanced shows every source; choosing a device here replaces this group.").variant(TextVariant::Muted));
            }
            group = group.child(
                SettingRow::new(
                    "Volume",
                    Text::new(
                        selected
                            .map(|s| {
                                format!("{:+.1} dB", s.source.controls.gain_millidb as f32 / 1000.0)
                            })
                            .unwrap_or_else(|| "0.0 dB".into()),
                    )
                    .variant(TextVariant::Caption),
                )
                .description("Adjust gain and balance per source in Advanced."),
            );
            if let Some(entry) = selected {
                group = group.child(SettingRow::new(
                    "Mute",
                    self.toggle(
                        format!("audio-easy-mute-{prefix}"),
                        format!("Mute {label}"),
                        entry.source.muted,
                        AudioSettingsEvent::Mute(entry.source.id.clone(), !entry.source.muted),
                        !entry.can_mute,
                    ),
                ));
            }
        }
        group.into_any_element()
    }
    fn processing(&self, source: &AudioSource) -> AnyElement {
        let c = source.controls;
        let disabled = !self.props.editable || self.props.testing;
        let gain = Stack::horizontal()
            .child(self.button(
                format!("audio-gain-down-{}", source.id.0),
                "−",
                AudioSettingsEvent::Controls(
                    source.id.clone(),
                    AudioSourceControls {
                        gain_millidb: (c.gain_millidb - 1000).max(-60000),
                        ..c
                    },
                ),
                disabled || c.gain_millidb <= -60000,
            ))
            .child(Text::new(format!(
                "{:+.1} dB",
                c.gain_millidb as f32 / 1000.0
            )))
            .child(self.button(
                format!("audio-gain-up-{}", source.id.0),
                "+",
                AudioSettingsEvent::Controls(
                    source.id.clone(),
                    AudioSourceControls {
                        gain_millidb: (c.gain_millidb + 1000).min(24000),
                        ..c
                    },
                ),
                disabled || c.gain_millidb >= 24000,
            ));
        let balance = Stack::horizontal()
            .child(self.button(
                format!("audio-balance-left-{}", source.id.0),
                "L",
                AudioSettingsEvent::Controls(
                    source.id.clone(),
                    AudioSourceControls {
                        balance: (c.balance - 10).max(-100),
                        ..c
                    },
                ),
                disabled || c.balance <= -100,
            ))
            .child(Text::new(if c.balance == 0 {
                "Center".into()
            } else {
                format!("{:+}%", c.balance)
            }))
            .child(self.button(
                format!("audio-balance-right-{}", source.id.0),
                "R",
                AudioSettingsEvent::Controls(
                    source.id.clone(),
                    AudioSourceControls {
                        balance: (c.balance + 10).min(100),
                        ..c
                    },
                ),
                disabled || c.balance >= 100,
            ));
        let mut reconnect = SegmentedControl::new();
        for (key, label, policy) in [
            ("default", "Default", AudioReconnectPolicy::ProcessDefault),
            ("silence", "Keep silent", AudioReconnectPolicy::Silence),
            ("exact", "Reconnect", AudioReconnectPolicy::ExactDevice),
        ] {
            reconnect = reconnect.child(
                self.button(
                    format!("audio-reconnect-{key}-{}", source.id.0),
                    label,
                    AudioSettingsEvent::Controls(
                        source.id.clone(),
                        AudioSourceControls {
                            reconnect: policy,
                            ..c
                        },
                    ),
                    disabled,
                )
                .segment()
                .selected(c.reconnect == policy),
            );
        }
        Stack::vertical().gap(Spacing::Xxs)
            .child(SettingRow::new("Gain", gain).description("Applies to Main and this source's individual track. Set before recording."))
            .child(SettingRow::new("Stereo balance", balance).description("Attenuates the opposite side; a mono stem keeps its mono layout."))
            .child(SettingRow::new("Device loss", reconnect).description("Reconnect retries the exact identity for up to 5 seconds. Other devices are never selected."))
            .into_any_element()
    }

    fn matrix(&self) -> AnyElement {
        use crate::audio_adapter::{individual, main, route};
        let mut matrix = Stack::vertical()
            .gap(Spacing::Xs)
            .child(Text::new("Source → Main mix / Individual track").variant(TextVariant::Label));
        for entry in &self.props.sources {
            let s = &entry.source;
            let disabled = !self.props.editable
                || !s.is_active
                || (s.kind == AudioSourceKind::Application && !self.props.per_app);
            let mut row = Stack::vertical()
                .gap(Spacing::Xxs)
                .child(Text::new(s.display_name.clone()).variant(TextVariant::Label))
                .child(Text::new(entry.connection.clone()).variant(TextVariant::Muted));
            let cells = div()
                .w_full()
                .flex()
                .items_center()
                .gap(Spacing::Sm.px())
                .child(SettingRow::new(
                    "Main",
                    self.toggle(
                        format!("audio-matrix-main-{}", s.id.0),
                        format!("{} to main mix", s.display_name),
                        main(s.route),
                        AudioSettingsEvent::Route(
                            s.id.clone(),
                            route(!main(s.route), individual(s.route)),
                        ),
                        disabled,
                    ),
                ))
                .child(SettingRow::new(
                    entry
                        .individual_track
                        .clone()
                        .unwrap_or_else(|| "Own track".into()),
                    self.toggle(
                        format!("audio-matrix-own-{}", s.id.0),
                        format!("{} to individual track", s.display_name),
                        individual(s.route),
                        AudioSettingsEvent::Route(
                            s.id.clone(),
                            route(main(s.route), !individual(s.route)),
                        ),
                        disabled,
                    ),
                ));
            row = row.child(cells).child(SettingRow::new(
                "Mute",
                self.toggle(
                    format!("audio-settings-mute-{}", s.id.0),
                    format!("Mute {}", s.display_name),
                    s.muted,
                    AudioSettingsEvent::Mute(s.id.clone(), !s.muted),
                    !entry.can_mute,
                ),
            ));
            if !s.is_active && s.route != AudioRoute::Ignore {
                row = row.child(self.button(
                    format!("audio-forget-{}", s.id.0),
                    "Turn off unavailable source",
                    AudioSettingsEvent::Route(s.id.clone(), AudioRoute::Ignore),
                    !self.props.editable,
                ));
            }
            row = row.child(self.processing(s));
            matrix = matrix.child(row);
        }
        if self.props.sources.is_empty() {
            matrix = matrix.child(
                Text::new(
                    "No audio sources. Connect a device or play application audio, then refresh.",
                )
                .variant(TextVariant::Muted),
            );
        }
        matrix.into_any_element()
    }
}
impl RenderOnce for AudioSettingsPanel {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let advanced = self.props.mode == AudioSettingsMode::Advanced;
        let switch = SegmentedControl::new()
            .id("audio-settings-mode")
            .selected(usize::from(advanced))
            .child(
                self.button(
                    "audio-mode-easy",
                    "Easy",
                    AudioSettingsEvent::Mode(AudioSettingsMode::Easy),
                    false,
                )
                .segment()
                .selected(!advanced),
            )
            .child(
                self.button(
                    "audio-mode-advanced",
                    "Advanced",
                    AudioSettingsEvent::Mode(AudioSettingsMode::Advanced),
                    false,
                )
                .segment()
                .selected(advanced),
            );
        let mut content = Stack::vertical()
            .gap(Spacing::Md)
            .child(switch)
            .child(Text::new(self.props.track_summary.clone()).variant(TextVariant::Muted));
        for issue in &self.props.issues {
            content = content.child(
                Text::new(issue.clone())
                    .variant(TextVariant::Muted)
                    .color(Color::Warning),
            );
        }
        if advanced {
            content = content.child(Text::new("Main mix plays as the first audio track. Individual tracks contain only their assigned source for editing.").variant(TextVariant::Muted))
                .child(self.matrix())

                .child(SettingRow::new("Channels", Text::new("Automatic")).description("Stereo mix; mono/stereo source layout preserved in individual tracks. Custom mapping is unavailable."))
                .child(SettingRow::new("Sample rate", Text::new("48 kHz · fixed")))
                .child(SettingRow::new("Codec", Text::new("Opus · fixed")))
                .child(SettingRow::new("Bitrate", Text::new("128 kb/s VBR per track")))
                .child(SettingRow::new("Microphone filters", Text::new("Unavailable")))
                .child(SettingRow::new("Monitoring", Text::new("Unavailable")).description("Audio level testing measures input without speaker playback."))
                .child(SettingRow::new("Sync offset", Text::new("Automatic")).description("Sources use the recording clock. Manual offsets are unavailable."))
                .child(Text::new(self.props.reconnect.clone()).variant(TextVariant::Muted));
            if self
                .props
                .sources
                .iter()
                .any(|s| s.source.id.0.starts_with("wasapi-session:"))
            {
                content = content.child(Text::new("Windows includes application child processes. Known tree/device aliases and desktop/application overlap are refused. Use microphone + desktop, or microphone + disjoint applications. For app + system, choose Other system audio excluding that app (all outputs). This requires Windows build 20348+. Protected or unreadable ancestry remains unknown.").variant(TextVariant::Muted));
            }
        } else {
            content = content
                .child(self.easy_device(
                    AudioSourceKind::System,
                    "System audio",
                    &self.props.system,
                ))
                .child(self.easy_device(
                    AudioSourceKind::Microphone,
                    "Microphone",
                    &self.props.microphone,
                ));
            let enabled: Vec<_> = self
                .props
                .sources
                .iter()
                .filter(|s| s.source.route != AudioRoute::Ignore)
                .collect();
            let separate = !enabled.is_empty()
                && enabled.iter().all(|s| {
                    matches!(
                        s.source.route,
                        AudioRoute::SeparateOnly | AudioRoute::MainMixAndSeparate
                    )
                });
            content = content.child(
                SettingRow::new(
                    "Separate audio tracks",
                    self.toggle(
                        "audio-easy-separate",
                        "Separate audio tracks",
                        separate,
                        AudioSettingsEvent::Separate(!separate),
                        !self.props.editable || enabled.is_empty(),
                    ),
                )
                .description("Keep the main mix and create one editable track per enabled source."),
            );
            if enabled
                .iter()
                .any(|s| s.source.kind == AudioSourceKind::Application)
            {
                content = content.child(
                    Text::new("Application audio is enabled. Manage its sources in Advanced.")
                        .variant(TextVariant::Muted),
                );
            }
        }
        if self
            .props
            .sources
            .iter()
            .any(|s| s.source.route != AudioRoute::Ignore)
            && !self.props.sources.iter().any(|s| {
                matches!(
                    s.source.route,
                    AudioRoute::MainMix | AudioRoute::MainMixAndSeparate
                )
            })
        {
            content = content.child(self.button(
                "audio-include-main",
                "Include enabled sources in main mix",
                AudioSettingsEvent::IncludeInMix,
                !self.props.editable,
            ));
        }
        content.child(
            Stack::horizontal()
                .child(self.button(
                    "audio-settings-refresh",
                    "Refresh devices",
                    AudioSettingsEvent::Refresh,
                    !self.props.editable || self.props.testing,
                ))
                .child(self.button(
                    "audio-settings-test",
                    if self.props.testing {
                        "Stop level test"
                    } else {
                        "Test audio levels"
                    },
                    AudioSettingsEvent::Test,
                    !self.props.can_test,
                )),
        )
    }
}
