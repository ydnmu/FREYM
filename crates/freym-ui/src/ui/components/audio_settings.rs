//! Audio settings use the existing recording configuration, routing and bounded menus.
use crate::ui::{
    components::{
        SegmentedControl, SettingRow, Toggle,
        video_settings::{VideoChoice, VideoDropdown},
    },
    primitives::*,
    theme::{colors::Color, spacing::Spacing},
};
use crate::video::VideoAction;
use frame_contracts::*;
use gpui::{
    AnyElement, App, ElementId, Entity, Focusable, IntoElement, RenderOnce, SharedString, Window,
    div, prelude::*,
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
    Output(AudioSettings),
    BitrateDraft(Option<String>),
    TrackMode(AudioRoute),
    Source(AudioSourceId),
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
    pub output_revision: u64,
    pub output: AudioSettings,
    pub track_mode: Option<AudioRoute>,
    pub selected_source: Option<AudioSourceId>,
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
struct BitrateInput {
    input: Entity<crate::input::TextInput>,
    committed: u32,
    revision: u64,
    error: Option<String>,
    draft: Option<String>,
    editing: bool,
    focus: gpui::FocusHandle,
    _observe: gpui::Subscription,
}
pub(crate) fn bitrate_value(value: &str) -> Result<u32, String> {
    let value = value
        .trim()
        .parse::<f64>()
        .map_err(|_| "Enter 6–510 kb/s.")?;
    let bits = value * 1000.;
    if !bits.is_finite()
        || !(6000. ..=510000.).contains(&bits)
        || (bits - bits.round()).abs() > 0.00001
    {
        return Err("Enter 6–510 kb/s, with at most three decimal places.".into());
    }
    Ok(bits.round() as u32)
}
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn output_summary(output: AudioSettings) -> String {
    let state = if output.validate().is_ok() {
        ""
    } else {
        " · unsupported saved output"
    };
    format!(
        "{:?} · {} kHz · {} kb/s VBR / track{state}",
        output.codec,
        output.sample_rate_hz as f64 / 1000.,
        output.bitrate_bps as f64 / 1000.
    )
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
            .on_click(move |_, w, cx| listener(event.clone(), w, cx))
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
            .on_click(move |_, w, cx| listener(event.clone(), w, cx))
    }
    fn dropdown(
        &self,
        id: impl Into<ElementId>,
        label: &'static str,
        value: impl Into<SharedString>,
        options: Vec<(String, AudioSettingsEvent, bool, bool)>,
    ) -> VideoDropdown {
        let listener = self.listener.clone();
        let disabled = !self.props.editable || options.is_empty();
        VideoDropdown::new(
            id,
            label,
            value,
            options
                .into_iter()
                .map(|(label, event, selected, enabled)| {
                    VideoChoice::new(label, VideoAction::Audio(event), selected, enabled)
                })
                .collect(),
        )
        .fill_width()
        .disabled(disabled)
        .on_select(move |action, w, cx| {
            if let VideoAction::Audio(event) = action {
                listener(event.clone(), w, cx)
            }
        })
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
        let selected = sources
            .iter()
            .find(|s| Some(&s.source.id) == selection.as_ref());
        let enabled = sources.iter().any(|s| s.source.route != AudioRoute::Ignore);
        let prefix = if kind == AudioSourceKind::Microphone {
            "microphone"
        } else {
            "system"
        };
        let value = selected
            .map(|s| s.source.display_name.clone())
            .unwrap_or_else(|| {
                if sources.is_empty() {
                    "No device available"
                } else {
                    "Saved device unavailable"
                }
                .into()
            });
        let choices = sources
            .iter()
            .map(|s| {
                (
                    format!(
                        "{}{}",
                        s.source.display_name,
                        if s.source.is_active {
                            ""
                        } else {
                            " · unavailable"
                        }
                    ),
                    AudioSettingsEvent::Device(kind, s.source.id.clone()),
                    Some(&s.source.id) == selection.as_ref(),
                    s.source.is_active,
                )
            })
            .collect();
        SettingRow::new(
            label,
            div()
                .w_full()
                .min_w_0()
                .flex()
                .items_center()
                .gap(gpui::px(10.))
                .child(div().flex_1().min_w_0().child(self.dropdown(
                    format!("audio-device-{prefix}"),
                    "Capture device",
                    value,
                    choices,
                )))
                .child(self.toggle(
                    format!("audio-easy-{prefix}"),
                    format!("Enable {label} capture"),
                    enabled,
                    AudioSettingsEvent::Enabled(kind, !enabled),
                    !self.props.editable
                        || sources.is_empty()
                        || (!enabled && selected.is_none_or(|s| !s.source.is_active)),
                )),
        )
        .description(
            if sources
                .iter()
                .filter(|s| s.source.route != AudioRoute::Ignore)
                .count()
                > 1
            {
                "Multiple sources enabled; selecting a device replaces this group."
            } else {
                "Enable this capture device."
            },
        )
        .into_any_element()
    }
    fn processing(&self, source: &AudioSource) -> AnyElement {
        let c = source.controls;
        let disabled = !self.props.editable || self.props.testing;
        let stepper = |prefix: &str,
                       value: String,
                       left: &str,
                       right: &str,
                       a: AudioSourceControls,
                       b: AudioSourceControls,
                       lo: bool,
                       hi: bool| {
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_end()
                .gap(gpui::px(6.))
                .child(self.button(
                    format!("audio-{prefix}-down-{}", source.id.0),
                    left,
                    AudioSettingsEvent::Controls(source.id.clone(), a),
                    disabled || lo,
                ))
                .child(
                    div()
                        .w(gpui::px(72.))
                        .text_align(gpui::TextAlign::Center)
                        .child(Text::new(value)),
                )
                .child(self.button(
                    format!("audio-{prefix}-up-{}", source.id.0),
                    right,
                    AudioSettingsEvent::Controls(source.id.clone(), b),
                    disabled || hi,
                ))
        };
        let gain = stepper(
            "gain",
            format!("{:+.1} dB", c.gain_millidb as f32 / 1000.),
            "−",
            "+",
            AudioSourceControls {
                gain_millidb: (c.gain_millidb - 1000).max(-60000),
                ..c
            },
            AudioSourceControls {
                gain_millidb: (c.gain_millidb + 1000).min(24000),
                ..c
            },
            c.gain_millidb <= -60000,
            c.gain_millidb >= 24000,
        );
        let balance = stepper(
            "balance",
            if c.balance == 0 {
                "Center".into()
            } else {
                format!("{:+}%", c.balance)
            },
            "L",
            "R",
            AudioSourceControls {
                balance: (c.balance - 10).max(-100),
                ..c
            },
            AudioSourceControls {
                balance: (c.balance + 10).min(100),
                ..c
            },
            c.balance <= -100,
            c.balance >= 100,
        );
        let policies = [
            (AudioReconnectPolicy::ProcessDefault, "Process default"),
            (AudioReconnectPolicy::Silence, "Keep silent"),
            (AudioReconnectPolicy::ExactDevice, "Reconnect same device"),
        ];
        let reconnect = self.dropdown(
            format!("audio-reconnect-{}", source.id.0),
            "Device loss",
            policies.iter().find(|(p, _)| *p == c.reconnect).unwrap().1,
            policies
                .iter()
                .map(|(policy, label)| {
                    (
                        (*label).into(),
                        AudioSettingsEvent::Controls(
                            source.id.clone(),
                            AudioSourceControls {
                                reconnect: *policy,
                                ..c
                            },
                        ),
                        c.reconnect == *policy,
                        true,
                    )
                })
                .collect(),
        );
        Stack::vertical()
            .gap(Spacing::Xs)
            .child(SettingRow::new("Gain", gain))
            .child(
                SettingRow::new("Balance", balance)
                    .description("Attenuates the opposite side; a preserved mono stem stays mono."),
            )
            .child(SettingRow::new("Device loss", reconnect).description(
                "Reconnect retries the exact identity for up to 5 seconds; no substitute device.",
            ))
            .into_any_element()
    }
    fn advanced(&self, window: &mut Window, cx: &mut App) -> AnyElement {
        let listener = self.listener.clone();
        let state = window.use_keyed_state("audio-bitrate-input", cx, |window, cx| {
            let input = cx.new(|cx| {
                crate::input::TextInput::new(
                    (self.props.output.bitrate_bps as f64 / 1000.).to_string(),
                    "kb/s",
                    cx,
                )
            });
            let observe = cx.observe_in(
                &input,
                window,
                move |state: &mut BitrateInput, input, window, cx| {
                    if !state.editing {
                        return;
                    }
                    let value = input.read(cx).value();
                    let message = match bitrate_value(value) {
                        Ok(bits) if bits == state.committed => None,
                        Ok(_) => Some("Apply or cancel the inline Opus bitrate edit.".into()),
                        Err(error) => Some(error),
                    };
                    if state.draft != message {
                        state.draft = message.clone();
                        listener(AudioSettingsEvent::BitrateDraft(message), window, cx);
                    }
                },
            );
            BitrateInput {
                input,
                committed: self.props.output.bitrate_bps,
                revision: self.props.output_revision,
                error: None,
                draft: None,
                editing: false,
                focus: cx.focus_handle().tab_stop(true),
                _observe: observe,
            }
        });
        if state.read(cx).committed != self.props.output.bitrate_bps
            || state.read(cx).revision != self.props.output_revision
        {
            state.update(cx, |s, cx| {
                s.committed = self.props.output.bitrate_bps;
                s.revision = self.props.output_revision;
                s.input.update(cx, |i, cx| {
                    i.set_value(
                        (self.props.output.bitrate_bps as f64 / 1000.).to_string(),
                        cx,
                    )
                });
                s.error = None;
                s.draft = None;
                s.editing = false;
            });
        }
        let submit = |s: &Entity<BitrateInput>,
                      output: AudioSettings,
                      listener: &Listener,
                      w: &mut Window,
                      cx: &mut App| {
            let text = s.read(cx).input.read(cx).value().to_owned();
            match bitrate_value(&text) {
                Ok(bits) => {
                    s.update(cx, |s, cx| {
                        s.error = None;
                        s.draft = None;
                        s.editing = false;
                        cx.notify();
                    });
                    listener(AudioSettingsEvent::BitrateDraft(None), w, cx);
                    let focus = s.read(cx).focus.clone();
                    w.focus(&focus, cx);
                    listener(
                        AudioSettingsEvent::Output(AudioSettings {
                            bitrate_bps: bits,
                            ..output
                        }),
                        w,
                        cx,
                    );
                }
                Err(e) => s.update(cx, |s, cx| {
                    s.error = Some(e);
                    cx.notify();
                }),
            }
        };
        let state_key = state.clone();
        let state_click = state.clone();
        let listener_key = self.listener.clone();
        let listener_click = self.listener.clone();
        let output = self.props.output;
        let editable = self.props.editable;
        let cancel_state = state.clone();
        let cancel_listener = self.listener.clone();
        let escape_state = state.clone();
        let escape_listener = self.listener.clone();
        let bitrate = div()
            .id("audio-bitrate-editor")
            .key_context("FREYMOverlay")
            .on_action(move |_: &DismissOverlay, w, cx| {
                cx.stop_propagation();
                escape_state.update(cx, |s, cx| {
                    s.error = None;
                    s.draft = None;
                    s.editing = false;
                    s.input.update(cx, |i, cx| {
                        i.set_value((s.committed as f64 / 1000.).to_string(), cx)
                    });
                    w.focus(&s.focus, cx);
                    cx.notify();
                });
                escape_listener(AudioSettingsEvent::BitrateDraft(None), w, cx);
            })
            .w_full()
            .min_w_0()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(gpui::px(6.))
            .child(
                div()
                    .id("audio-bitrate-input")
                    .debug_selector(|| "audio-bitrate-input".into())
                    .w(gpui::px(96.))
                    .flex_shrink_0()
                    .min_w_0()
                    .child(if self.props.editable {
                        state.read(cx).input.clone().into_any_element()
                    } else {
                        Text::new((output.bitrate_bps as f64 / 1000.).to_string())
                            .into_any_element()
                    }),
            )
            .child(Text::new("kb/s").variant(TextVariant::Caption))
            .child(
                Button::new("audio-bitrate-apply", "Apply")
                    .size(ButtonSize::Compact)
                    .disabled(!editable)
                    .on_click(move |_, w, cx| submit(&state_click, output, &listener_click, w, cx)),
            )
            .child(
                Button::new("audio-bitrate-cancel", "Cancel bitrate edit")
                    .size(ButtonSize::ListIcon)
                    .icon(Icons::Close)
                    .disabled(!editable)
                    .on_click(move |_, w, cx| {
                        cancel_state.update(cx, |s, cx| {
                            s.error = None;
                            s.draft = None;
                            s.editing = false;
                            s.input.update(cx, |i, cx| {
                                i.set_value((s.committed as f64 / 1000.).to_string(), cx)
                            });
                            w.focus(&s.focus, cx);
                            cx.notify();
                        });
                        cancel_listener(AudioSettingsEvent::BitrateDraft(None), w, cx);
                    }),
            )
            .capture_key_down(move |event, w, cx| {
                if editable && event.keystroke.key == "enter" && !event.is_held {
                    submit(&state_key, output, &listener_key, w, cx);
                    cx.stop_propagation();
                }
            });
        let bitrate = if state.read(cx).editing {
            bitrate.into_any_element()
        } else {
            let edit = state.clone();
            Button::new(
                "audio-bitrate-edit",
                format!("{} kb/s", output.bitrate_bps as f64 / 1000.),
            )
            .size(ButtonSize::Compact)
            .focus(state.read(cx).focus.clone())
            .disabled(!editable)
            .on_click(move |_, w, cx| {
                edit.update(cx, |s, cx| {
                    s.editing = true;
                    s.error = None;
                    w.focus(&s.input.focus_handle(cx), cx);
                    cx.notify();
                })
            })
            .into_any_element()
        };
        let mut content = Stack::vertical()
            .gap(Spacing::Xs)
            .child(SettingRow::new("Opus bitrate", bitrate).description("6–510 kb/s per track."));
        if let Some(error) = &state.read(cx).error {
            content = content.child(
                Text::new(error.clone())
                    .variant(TextVariant::Caption)
                    .color(Color::Error),
            );
        }
        let selected = self
            .props
            .sources
            .iter()
            .find(|s| Some(&s.source.id) == self.props.selected_source.as_ref())
            .or_else(|| {
                self.props
                    .sources
                    .iter()
                    .find(|s| s.source.route != AudioRoute::Ignore)
            })
            .or(self.props.sources.first());
        let entries = self
            .props
            .sources
            .iter()
            .map(|s| {
                (
                    s.source.display_name.clone(),
                    AudioSettingsEvent::Source(s.source.id.clone()),
                    selected.is_some_and(|selected| selected.source.id == s.source.id),
                    true,
                )
            })
            .collect();
        content = content.child(SettingRow::new(
            "Source",
            self.dropdown(
                "audio-advanced-source",
                "Audio source",
                selected
                    .map(|s| s.source.display_name.clone())
                    .unwrap_or_else(|| "No sources".into()),
                entries,
            ),
        ));
        if let Some(entry) = selected {
            let source = &entry.source;
            let routes = [
                (AudioRoute::Ignore, "Off · not captured"),
                (AudioRoute::MainMix, "Mixed"),
                (AudioRoute::SeparateOnly, "Separate"),
                (AudioRoute::MainMixAndSeparate, "Both"),
            ];
            content = content
                .child(SettingRow::new(
                    "Track destination",
                    self.dropdown(
                        "audio-source-route",
                        "Track destination",
                        routes.iter().find(|(r, _)| *r == source.route).unwrap().1,
                        routes
                            .iter()
                            .map(|(route, label)| {
                                (
                                    (*label).into(),
                                    AudioSettingsEvent::Route(source.id.clone(), *route),
                                    source.route == *route,
                                    *route == AudioRoute::Ignore
                                        || source.is_active
                                            && (source.kind != AudioSourceKind::Application
                                                || self.props.per_app),
                                )
                            })
                            .collect(),
                    ),
                ))
                .child(self.processing(source));
        }
        content.into_any_element()
    }
}

impl RenderOnce for AudioSettingsPanel {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let advanced = self.props.mode == AudioSettingsMode::Advanced;
        let switch = SegmentedControl::new()
            .id("audio-settings-mode")
            .compact()
            .selected(usize::from(advanced))
            .children(
                [
                    (AudioSettingsMode::Easy, "Easy"),
                    (AudioSettingsMode::Advanced, "Advanced"),
                ]
                .map(|(mode, label)| {
                    self.button(
                        if mode == AudioSettingsMode::Easy {
                            "audio-mode-easy"
                        } else {
                            "audio-mode-advanced"
                        },
                        label,
                        AudioSettingsEvent::Mode(mode),
                        false,
                    )
                    .segment()
                    .selected(self.props.mode == mode)
                }),
            );
        let mut content = Stack::vertical().gap(Spacing::Xs).child(
            div()
                .id("audio-mode-header")
                .flex_shrink_0()
                .min_h(gpui::px(40.))
                .mb(gpui::px(8.))
                .debug_selector(|| "audio-mode-header".into())
                .w_full()
                .flex()
                .items_center()
                .flex_wrap()
                .justify_between()
                .gap(gpui::px(12.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .debug_selector(|| "audio-mode-heading".into())
                        .child(Text::new("Audio").variant(TextVariant::Heading)),
                )
                .child(
                    self.button(
                        "audio-settings-refresh",
                        "Refresh devices",
                        AudioSettingsEvent::Refresh,
                        !self.props.editable || self.props.testing,
                    )
                    .icon(Icons::Refresh),
                )
                .child(
                    div()
                        .id("audio-mode-control")
                        .debug_selector(|| "audio-mode-control".into())
                        .w(gpui::px(220.))
                        .max_w(gpui::relative(0.62))
                        .flex_shrink_0()
                        .child(switch),
                ),
        );
        let presets = [
            (AudioQuality::Voice, "Voice · 64 kb/s"),
            (AudioQuality::Standard, "Standard · 128 kb/s"),
            (AudioQuality::High, "High · 192 kb/s"),
            (AudioQuality::Best, "Best · 256 kb/s"),
            (AudioQuality::Custom, "Custom…"),
        ];
        if !advanced {
            let quality = self.props.output.quality();
            content = content.child(SettingRow::new(
                "Audio quality",
                self.dropdown(
                    "audio-quality",
                    "Audio quality",
                    presets.iter().find(|(p, _)| *p == quality).unwrap().1,
                    presets
                        .iter()
                        .map(|(p, label)| {
                            let mut output = self.props.output;
                            output.set_quality(*p);
                            (
                                (*label).into(),
                                if *p == AudioQuality::Custom {
                                    AudioSettingsEvent::Mode(AudioSettingsMode::Advanced)
                                } else {
                                    AudioSettingsEvent::Output(output)
                                },
                                quality == *p,
                                true,
                            )
                        })
                        .collect(),
                ),
            ));
            let mut tracks = SegmentedControl::new().id("audio-track-mode").compact();
            if let Some(mode) = self.props.track_mode {
                tracks = tracks.selected(match mode {
                    AudioRoute::SeparateOnly => 1,
                    AudioRoute::MainMixAndSeparate => 2,
                    _ => 0,
                });
            }
            for (route, label) in [
                (AudioRoute::MainMix, "Mixed"),
                (AudioRoute::SeparateOnly, "Separate"),
                (AudioRoute::MainMixAndSeparate, "Both"),
            ] {
                tracks = tracks.child(
                    self.button(
                        format!("audio-track-{label}"),
                        label,
                        AudioSettingsEvent::TrackMode(route),
                        !self.props.editable,
                    )
                    .segment()
                    .selected(self.props.track_mode == Some(route)),
                );
            }
            content = content.child(SettingRow::new("Track mode", tracks));
            if self.props.track_mode.is_none() {
                content=content.child(Text::new("Custom per-source routing; choose a mode to apply it to all enabled sources.").variant(TextVariant::Caption));
            }
            let channels = [
                (AudioChannels::Preserve, "Preserve"),
                (AudioChannels::Mono, "Mono"),
                (AudioChannels::Stereo, "Stereo"),
            ];
            content = content
                .child(SettingRow::new(
                    "Channels",
                    self.dropdown(
                        "audio-channels",
                        "Output channels",
                        channels
                            .iter()
                            .find(|(v, _)| *v == self.props.output.channels)
                            .unwrap()
                            .1,
                        channels
                            .iter()
                            .map(|(v, label)| {
                                (
                                    (*label).into(),
                                    AudioSettingsEvent::Output(AudioSettings {
                                        channels: *v,
                                        ..self.props.output
                                    }),
                                    *v == self.props.output.channels,
                                    true,
                                )
                            })
                            .collect(),
                    ),
                ))
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
        } else {
            content = content.child(self.advanced(window, cx));
        }
        for issue in &self.props.issues {
            content = content.child(
                Text::new(issue.clone())
                    .variant(TextVariant::Caption)
                    .color(Color::Warning),
            );
        }
        content
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn followup_bitrate_validation_uses_real_bounds_and_never_fabricates_output() {
        for (s, v) in [("6", 6000), ("510", 510000), ("96.5", 96500)] {
            assert_eq!(bitrate_value(s).unwrap(), v);
        }
        for s in [
            "NaN", "inf", "5.999", "510.001", "0", "-1", "96.0001", "abc",
        ] {
            assert!(bitrate_value(s).is_err(), "{s}");
        }
        assert_eq!(
            output_summary(AudioSettings::default()),
            "Opus · 48 kHz · 128 kb/s VBR / track"
        );
        assert!(
            output_summary(AudioSettings {
                codec: AudioCodec::Aac,
                ..Default::default()
            })
            .contains("unsupported")
        );
    }
}
