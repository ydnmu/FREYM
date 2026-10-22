use super::Toggle;
use crate::ui::{
    primitives::{Button, ButtonSize, Icon, Icons, Stack, Text, TextVariant, clickable},
    theme::{
        colors::Color,
        spacing::{Layout, Spacing},
    },
};
use gpui::{
    AnimationExt, AnyElement, App, ElementId, IntoElement, RenderOnce, SharedString, Window, div,
    prelude::*, px,
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioKind {
    Microphone,
    System,
    Application,
}
#[derive(IntoElement)]
pub struct AudioSourceRow {
    button: Button,
    label: SharedString,
    kind: AudioKind,
    enabled: bool,
    connected: bool,
    device: Option<SharedString>,
    route: Option<SharedString>,
    separate_track: bool,
    meter: Option<AnyElement>,
    compact: bool,
    controls: Option<AnyElement>,
    inline: bool,
    expanded: bool,
}
impl AudioSourceRow {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>, kind: AudioKind) -> Self {
        let label = label.into();
        Self {
            button: Button::new(id, label.clone())
                .variant(crate::ui::primitives::ButtonVariant::Ghost)
                .size(ButtonSize::Row),
            label,
            kind,
            enabled: false,
            connected: true,
            device: None,
            route: None,
            separate_track: false,
            meter: None,
            compact: false,
            controls: None,
            inline: false,
            expanded: false,
        }
    }
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
    pub fn expanded(mut self, expanded: bool) -> Self {
        self.expanded = expanded;
        self
    }
    pub fn connected(mut self, connected: bool) -> Self {
        self.connected = connected;
        self
    }
    pub fn device(mut self, device: impl Into<SharedString>) -> Self {
        self.device = Some(device.into());
        self
    }
    pub fn route(mut self, route: impl Into<SharedString>) -> Self {
        self.route = Some(route.into());
        self
    }
    pub fn separate_track(mut self, separate: bool) -> Self {
        self.separate_track = separate;
        self
    }
    pub fn meter(mut self, meter: impl IntoElement) -> Self {
        self.meter = Some(meter.into_any_element());
        self
    }
    pub fn compact(mut self) -> Self {
        self.compact = true;
        self
    }
    pub fn controls(mut self, controls: impl IntoElement) -> Self {
        self.controls = Some(controls.into_any_element());
        self
    }
    /// Recorder summary: one source row with its existing routing controls.
    pub fn inline(mut self) -> Self {
        self.inline = true;
        self
    }
}
clickable!(AudioSourceRow);
impl RenderOnce for AudioSourceRow {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let entry_id = format!("audio-source-entry-{}", self.button.id());
        let icon = if self.kind == AudioKind::Microphone {
            Icons::Mic
        } else {
            Icons::Audio
        };
        let mut labels = Stack::vertical()
            .gap(Spacing::Xxs)
            .child(Text::new(self.label).variant(TextVariant::Label).truncate());
        if let Some(device) = self.device.filter(|_| !self.inline) {
            labels = labels.child(Text::new(device).variant(TextVariant::Muted).truncate());
        }
        if !self.connected && !self.inline {
            labels = labels.child(
                Text::new("Disconnected")
                    .variant(TextVariant::Caption)
                    .color(Color::Warning),
            );
        }
        let mut row = div()
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap(Spacing::Sm.px())
            .child(Icon::new(icon))
            .child(div().flex_1().min_w_0().child(labels));
        row = row.child(Toggle::indicator(self.enabled));
        if let Some(route) = self.route.clone() {
            row = row.child(
                Text::new(route)
                    .variant(TextVariant::Caption)
                    .color(Color::Accent),
            );
        }
        if self.separate_track && self.route.is_none() {
            row = row.child(Text::new("Separate track").variant(TextVariant::Muted));
        }
        if self.inline {
            let mut summary = div()
                .flex()
                .items_center()
                .child(div().flex_1().min_w_0().child(self.button.content(row)));
            let mut details = None;
            if let Some(controls) = self.controls {
                if self.expanded {
                    details = Some(controls);
                } else {
                    summary = summary.child(controls);
                }
            }
            return div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .border_b_1()
                .border_color(Color::SubtleBorder.get())
                .child(summary)
                .when_some(details, |d, controls| {
                    d.child(
                        div()
                            .px(Spacing::Xs.px())
                            .pb(Spacing::Xs.px())
                            .child(controls)
                            .with_animation(
                                entry_id,
                                crate::ui::theme::motion::entrance(),
                                |d, t| {
                                    d.relative()
                                        .left(px((1. - t) * crate::ui::theme::motion::PANEL_TRAVEL))
                                        .opacity(t)
                                },
                            ),
                    )
                });
        }
        let mut group = div()
            .min_w(px(Layout::AUDIO_MIN_WIDTH))
            .flex()
            .flex_col()
            .gap(Spacing::Xxs.px())
            .child(self.button.content(row));
        if let Some(meter) = self.meter {
            group = group.child(div().ml(Spacing::Xl.px()).child(meter));
        }
        if let Some(controls) = self.controls {
            group = group.child(controls);
        }
        if !self.compact {
            group = group.w_full();
        }
        group
    }
}
