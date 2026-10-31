use gpui::{AnyElement, App, IntoElement, ParentElement, RenderOnce, Window, div, prelude::*, px};

#[derive(IntoElement)]
pub struct RecorderAudioPanel {
    sources: Vec<AnyElement>,
}
impl Default for RecorderAudioPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl RecorderAudioPanel {
    pub fn new() -> Self {
        Self { sources: vec![] }
    }
}
impl ParentElement for RecorderAudioPanel {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.sources.extend(elements);
    }
}
impl RenderOnce for RecorderAudioPanel {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let element = div()
            .id("audio-panel")
            .debug_selector(|| "audio-panel".into())
            .size_full()
            .min_w_0()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(
                div()
                    .id("home-audio-sources")
                    .debug_selector(|| "home-audio-sources".into())
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scroll()
                    .px(px(6.))
                    .pt(px(6.))
                    .pb(px(8.))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .children(
                        self.sources
                            .into_iter()
                            .map(|s| div().flex_shrink_0().child(s)),
                    ),
            );
        crate::ui::finish_design(
            crate::ui::designable(element, "audio-panel", "Audio panel", cx),
            "audio-panel",
            cx,
        )
    }
}
