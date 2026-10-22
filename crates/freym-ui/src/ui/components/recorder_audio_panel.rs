use crate::ui::{
    primitives::{Button, ButtonSize, Icon, Icons},
    theme::{colors::Color, motion},
};
use gpui::{
    AnimationExt, AnyElement, App, IntoElement, ParentElement, RenderOnce, Window, div, prelude::*,
    px,
};

#[derive(IntoElement)]
pub struct RecorderAudioPanel {
    heading: Button,
    expanded: bool,
    settings: AnyElement,
    test: AnyElement,
    meters: AnyElement,
    sources: Vec<AnyElement>,
}
impl RecorderAudioPanel {
    pub fn new(
        settings: impl IntoElement,
        test: impl IntoElement,
        meters: impl IntoElement,
    ) -> Self {
        Self {
            heading: Button::new("audio-expand", "Expand Audio")
                .variant(crate::ui::primitives::ButtonVariant::Ghost)
                .size(ButtonSize::Row),
            expanded: false,
            settings: settings.into_any_element(),
            test: test.into_any_element(),
            meters: meters.into_any_element(),
            sources: vec![],
        }
    }
    pub fn expanded(mut self, expanded: bool) -> Self {
        self.expanded = expanded;
        self.heading = self.heading.label(if expanded {
            "Collapse Audio"
        } else {
            "Expand Audio"
        });
        self
    }
    pub fn on_click(
        mut self,
        listener: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.heading = self.heading.on_click(listener);
        self
    }
}
impl ParentElement for RecorderAudioPanel {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.sources.extend(elements);
    }
}
impl RenderOnce for RecorderAudioPanel {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        div()
            .id("audio-panel")
            .debug_selector(|| "audio-panel".into())
            .size_full()
            .min_w_0()
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(8.))
            .border_1()
            .border_color(Color::DefaultBorder.get())
            .bg(Color::PrimarySurface.get())
            .child(
                div().h(px(44.)).flex_shrink_0().child(
                    self.heading.expanded(self.expanded).content(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(Icon::new(Icons::Audio))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(14.))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("Audio"),
                            )
                            .child(Icon::new(Icons::Chevron).rotation(if self.expanded {
                                180.
                            } else {
                                0.
                            })),
                    ),
                ),
            )
            .child(
                div()
                    .id("home-audio-sources")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p(px(8.))
                    .flex()
                    .flex_col()
                    .children(self.sources),
            )
            .when(self.expanded, |d| {
                d.child(
                    div()
                        .id("audio-details")
                        .debug_selector(|| "audio-details".into())
                        .flex_shrink_0()
                        .border_t_1()
                        .border_color(Color::SubtleBorder.get())
                        .p(px(8.))
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .child(self.meters)
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(self.test)
                                .child(self.settings),
                        )
                        .with_animation("audio-details-entry", motion::entrance(), |d, t| {
                            d.relative()
                                .left(px((1. - t) * motion::PANEL_TRAVEL))
                                .opacity(t)
                        }),
                )
            })
    }
}
