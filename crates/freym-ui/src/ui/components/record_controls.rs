use super::RecordButton;
use gpui::{AnyElement, App, IntoElement, RenderOnce, SharedString, Window, div, prelude::*, px};
#[derive(IntoElement)]
pub struct RecordControls {
    record: RecordButton,
    session: Option<(AnyElement, SharedString)>,
    accessory: Option<AnyElement>,
}
impl RecordControls {
    pub fn new(record: RecordButton) -> Self {
        Self {
            record,
            session: None,
            accessory: None,
        }
    }
    pub fn accessory(mut self, accessory: impl IntoElement) -> Self {
        self.accessory = Some(accessory.into_any_element());
        self
    }
    pub fn session(mut self, pause: impl IntoElement, elapsed: impl Into<SharedString>) -> Self {
        self.session = Some((pause.into_any_element(), elapsed.into()));
        self
    }
}
impl RenderOnce for RecordControls {
    fn render(self, window: &mut Window, _: &mut App) -> impl IntoElement {
        let compact = f32::from(window.viewport_size().width) < 1100.;
        let record = if let Some((pause, _)) = self.session {
            div()
                .w_full()
                .flex()
                .gap(px(8.))
                .child(div().flex_1().min_w_0().child(pause))
                .child(
                    div()
                        .w(px(54.))
                        .flex_shrink_0()
                        .child(self.record.icon_only()),
                )
        } else {
            div().w_full().child(self.record)
        };
        div()
            .id("recorder-transport")
            .w_full()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .w_full()
                    .flex()
                    .when(compact, |d| d.flex_col())
                    .items_center()
                    .gap(px(4.))
                    .child(
                        div()
                            .when(!compact, |d| d.flex_1())
                            .when(compact, |d| d.w_full())
                            .min_w_0()
                            .child(record),
                    )
                    .when_some(self.accessory, |d, a| {
                        d.child(div().flex_shrink_0().child(a))
                    }),
            )
    }
}
