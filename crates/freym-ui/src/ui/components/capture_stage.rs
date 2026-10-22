use crate::ui::{
    primitives::{Icon, IconSize, Icons, Text, TextVariant},
    theme::{colors::Color, radius::Radius, spacing::Spacing},
};
use gpui::{AnyElement, App, IntoElement, RenderOnce, SharedString, Window, div, prelude::*};

/// Source summary and native preview entry point. No demonstration image or stream.
#[derive(IntoElement)]
pub struct CaptureStage {
    source: SharedString,
    explanation: SharedString,
    preview: AnyElement,
    picker: AnyElement,
    content: Option<AnyElement>,
}
impl CaptureStage {
    pub fn new(
        source: impl Into<SharedString>,
        explanation: impl Into<SharedString>,
        preview: impl IntoElement,
        picker: impl IntoElement,
    ) -> Self {
        Self {
            source: source.into(),
            explanation: explanation.into(),
            preview: preview.into_any_element(),
            picker: picker.into_any_element(),
            content: None,
        }
    }
    pub fn embedded(mut self, content: impl IntoElement) -> Self {
        self.content = Some(content.into_any_element());
        self
    }
}
impl RenderOnce for CaptureStage {
    fn render(self, window: &mut Window, _: &mut App) -> impl IntoElement {
        let compact = f32::from(window.viewport_size().width) < 1100.;
        div()
            .id("capture-stage")
            .debug_selector(|| "capture-stage".into())
            .size_full()
            .relative()
            .min_w_0()
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(gpui::px(8.))
            .border_2()
            .border_color(Color::DefaultBorder.get())
            .when(self.content.is_none(), |d| {
                d.bg(Color::PreviewSurface.get())
            })
            .child(if let Some(content) = self.content {
                div().flex_1().min_h_0().child(content)
            } else {
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .p(Spacing::Sm.px())
                    .pb(gpui::px(52.))
                    .gap(Spacing::Sm.px())
                    .child(
                        div()
                            .p(Spacing::Xs.px())
                            .rounded(Radius::Large.px())
                            .bg(Color::PrimarySurface.get())
                            .border_1()
                            .border_color(Color::SubtleBorder.get())
                            .child(
                                Icon::new(Icons::Screen)
                                    .size(IconSize::Large)
                                    .color(Color::Accent),
                            ),
                    )
                    .when(!compact, |d| {
                        d.child(Text::new("Capture preview").variant(TextVariant::Heading))
                    })
                    .child(Text::new(self.explanation).variant(TextVariant::Caption))
                    .child(self.preview)
            })
            .child(
                div()
                    .absolute()
                    .left(gpui::px(10.))
                    .right(gpui::px(10.))
                    .bottom(gpui::px(10.))
                    .flex_shrink_0()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(Spacing::Xs.px())
                    .py(gpui::px(4.))
                    .gap(Spacing::Xs.px())
                    .rounded(gpui::px(8.))
                    .bg(Color::PrimarySurface.get().opacity(0.9))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(Spacing::Xxs.px())
                            .child(
                                Text::new(self.source)
                                    .variant(TextVariant::Label)
                                    .truncate(),
                            ),
                    )
                    .child(self.picker),
            )
            .into_any_element()
    }
}
