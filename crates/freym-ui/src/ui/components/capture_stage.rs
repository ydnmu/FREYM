use crate::ui::{
    primitives::{Icon, IconSize, Icons, Text, TextVariant},
    theme::{colors::Color, radius::Radius, spacing::Spacing},
};
use gpui::{AnyElement, App, IntoElement, RenderOnce, SharedString, Window, div, prelude::*};

/// Source summary and native preview entry point. No demonstration image or stream.
#[derive(IntoElement)]
pub struct CaptureStage {
    explanation: SharedString,
    content: Option<AnyElement>,
}
impl CaptureStage {
    pub fn new(explanation: impl Into<SharedString>) -> Self {
        Self {
            explanation: explanation.into(),
            content: None,
        }
    }
    pub fn embedded(mut self, content: impl IntoElement) -> Self {
        self.content = Some(content.into_any_element());
        self
    }
}
impl RenderOnce for CaptureStage {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let compact = f32::from(crate::ui::viewport(window, cx).width) < 1100.;
        let element = div()
            .id("capture-stage")
            .debug_selector(|| "capture-stage".into())
            .size_full()
            .relative()
            .min_w_0()
            .flex()
            .flex_col()
            .overflow_hidden()
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
                    .child(
                        Text::new(self.explanation)
                            .variant(TextVariant::Caption)
                            .color(Color::SecondaryText),
                    )
            });
        crate::ui::finish_design(
            crate::ui::designable(element, "capture-stage", "Capture preview", cx)
                .rounded(gpui::px(0.))
                .border_0()
                .shadow(vec![]),
            "capture-stage",
            cx,
        )
    }
}
