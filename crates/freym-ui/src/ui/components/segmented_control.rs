use crate::ui::theme::{colors::Color, motion, radius::Radius, spacing::Spacing};
use gpui::{
    AnimationExt, AnyElement, App, ElementId, IntoElement, ParentElement, RenderOnce, Window, div,
    prelude::*, relative,
};
#[derive(IntoElement, Default)]
pub struct SegmentedControl {
    segments: Vec<AnyElement>,
    id: Option<ElementId>,
    selected: Option<usize>,
}
impl SegmentedControl {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn id(mut self, id: impl Into<ElementId>) -> Self {
        self.id = Some(id.into());
        self
    }
    pub fn selected(mut self, selected: usize) -> Self {
        self.selected = Some(selected);
        self
    }
}
impl ParentElement for SegmentedControl {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.segments.extend(elements);
    }
}
impl RenderOnce for SegmentedControl {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let root = div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap(Spacing::Xxs.px())
            .p(Spacing::Xxs.px())
            .rounded(Radius::Large.px())
            .bg(Color::Accent.get().opacity(0.055));
        if let Some(id) = self.id {
            let count = self.segments.len();
            let root = root.id(id.clone()).w_full();
            let mut track = div().relative().w_full().flex().items_center();
            if let Some(selected) = self.selected.filter(|i| *i < count) {
                let fraction = 1. / count as f32;
                let debug = format!("segment-indicator-{id}");
                track = track.child(
                    div()
                        .id("selection-surface")
                        .debug_selector(move || debug.clone())
                        .absolute()
                        .top_0()
                        .h_full()
                        .w(relative(fraction))
                        .rounded(Radius::Medium.px())
                        .border_1()
                        .border_color(Color::DefaultBorder.get())
                        .bg(if crate::ui::theme::ThemeId::current().is_motion() {
                            Color::Accent.get()
                        } else {
                            Color::ElevatedSurface.get()
                        })
                        .when(crate::ui::theme::ThemeId::current().is_motion(), |d| {
                            d.rounded(gpui::px(3.))
                        })
                        .with_spring(
                            "selection-position",
                            motion::selection(selected as f32),
                            move |d, position| {
                                d.left(relative(
                                    position.clamp(0., count.saturating_sub(1) as f32) * fraction,
                                ))
                            },
                        ),
                );
            }
            root.child(
                track.children(
                    self.segments
                        .into_iter()
                        .map(|segment| div().relative().flex_1().min_w_0().child(segment)),
                ),
            )
            .into_any_element()
        } else {
            root.children(self.segments).into_any_element()
        }
    }
}
