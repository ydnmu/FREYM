use super::Toggle;
use crate::ui::{
    primitives::{Button, ButtonSize, Icon, Icons, Stack, Text, TextVariant, clickable},
    theme::{
        colors::Color,
        spacing::{Layout, Spacing},
    },
};
use gpui::{
    AnyElement, App, ElementId, IntoElement, RenderOnce, SharedString, Window, div, prelude::*, px,
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
    drag_handle: Option<AnyElement>,
    available_width: Option<f32>,
}
impl AudioSourceRow {
    pub(crate) fn available_width(mut self, width: f32) -> Self {
        self.available_width = Some(width);
        self
    }
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
            drag_handle: None,
            available_width: None,
        }
    }
    pub(crate) fn live_mute(mut self, muted: Option<bool>) -> Self {
        if let Some(muted) = muted {
            self.enabled = !muted;
            let label = format!("{} {}", if muted { "Unmute" } else { "Mute" }, self.label);
            self.button = self.button.label(label.clone()).tooltip(label);
        }
        self
    }
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
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
    pub(crate) fn drag_handle(mut self, handle: impl IntoElement) -> Self {
        self.drag_handle = Some(handle.into_any_element());
        self
    }
}
clickable!(AudioSourceRow);
impl RenderOnce for AudioSourceRow {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let viewport = crate::ui::viewport(window, cx);
        let label_width = (self.available_width.unwrap_or_else(|| {
            super::RecorderGeometry::new(viewport.width.into(), viewport.height.into())
                .audio_width()
        }) * 0.25)
            .clamp(102., 220.)
            - if self.inline { 26. } else { 0. };
        let has_ribbon = self.inline && self.meter.is_some();
        let entry_id = format!("audio-source-entry-{}", self.button.id());
        let icon = match self.kind {
            AudioKind::Microphone => Icons::Mic,
            AudioKind::Application if self.inline => Icons::Window,
            _ => Icons::Audio,
        };
        let mut labels = Stack::vertical().gap(Spacing::Xxs).child(if self.inline {
            div()
                .truncate()
                .text_size(px(13.))
                .line_height(px(18.))
                .font_weight(gpui::FontWeight::MEDIUM)
                .child(self.label)
                .into_any_element()
        } else {
            Text::new(self.label)
                .variant(TextVariant::Label)
                .truncate()
                .into_any_element()
        });
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
        let icon_id = format!("{entry_id}-icon");
        let label_id = format!("{entry_id}-label");
        let switch_id = format!("{entry_id}-switch");
        let mut row = div()
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(if self.inline { 8. } else { Spacing::Sm.value() }))
            .child(
                div()
                    .id(icon_id.clone())
                    .debug_selector(move || icon_id)
                    .size(px(18.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .opacity(if !self.inline {
                        1.
                    } else if !self.enabled || !self.connected {
                        0.32
                    } else {
                        0.68
                    })
                    .child(if self.inline {
                        Icon::new(icon).color(Color::SecondaryText)
                    } else {
                        Icon::new(icon)
                    }),
            )
            .child(
                div()
                    .id(label_id.clone())
                    .debug_selector(move || label_id)
                    .flex_1()
                    .min_w_0()
                    .when(has_ribbon, |d| d.flex_none().w(px(label_width)))
                    .child(labels),
            );
        let mut meter = self.meter;
        if self.inline
            && let Some(ribbon) = meter.take()
        {
            row = row.child(div().flex_1().min_w_0().h(px(22.)).child(ribbon));
        }
        row = row.child(
            div()
                .id(switch_id.clone())
                .debug_selector(move || switch_id)
                .w(px(if self.inline {
                    Layout::TOGGLE_SMALL_WIDTH
                } else {
                    Layout::TOGGLE_WIDTH
                }))
                .h(px(if self.inline {
                    Layout::TOGGLE_SMALL_HEIGHT
                } else {
                    Layout::TOGGLE_HEIGHT
                }))
                .flex_shrink_0()
                .when(self.inline, |d| {
                    d.child(Toggle::small_indicator(self.enabled))
                })
                .when(!self.inline, |d| d.child(Toggle::indicator(self.enabled))),
        );
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
                .id(entry_id.clone())
                .debug_selector(move || entry_id.clone())
                .group(gpui::SharedString::from(format!(
                    "audio-source-entry-{}",
                    self.button.id()
                )))
                .w_full()
                .h(px(36.))
                .flex_shrink_0()
                .min_w_0()
                .rounded(px(6.))
                .bg(Color::RowSurface.get())
                .hover(|d| d.bg(Color::HoverSurface.get()))
                .flex()
                .items_center()
                .when(self.drag_handle.is_some(), |d| d.pr(px(8.)))
                .child(
                    div().min_w_0().flex_1().child(
                        self.button
                            .size(ButtonSize::ListRow)
                            .toggled(self.enabled)
                            .content(row),
                    ),
                );
            if let Some(controls) = self.controls {
                summary = summary.child(
                    div()
                        .flex()
                        .flex_shrink_0()
                        .justify_end()
                        .pl(px(4.))
                        .child(controls),
                );
            }
            if let Some(handle) = self.drag_handle {
                summary = summary.child(handle);
            }
            return summary.into_any_element();
        }
        let mut group = div()
            .min_w(px(Layout::AUDIO_MIN_WIDTH))
            .flex()
            .flex_col()
            .gap(Spacing::Xxs.px())
            .child(self.button.content(row));
        if let Some(meter) = meter {
            group = group.child(div().ml(Spacing::Xl.px()).child(meter));
        }
        if let Some(controls) = self.controls {
            group = group.child(controls);
        }
        if !self.compact {
            group = group.w_full();
        }
        group.into_any_element()
    }
}
