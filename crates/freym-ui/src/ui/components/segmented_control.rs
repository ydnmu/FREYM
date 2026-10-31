use crate::ui::primitives::motion_surface;
use crate::ui::theme::{
    colors::Color,
    controls::{self, Phase, Role},
    motion,
    radius::Radius,
    spacing::Spacing,
};
use gpui::{
    AnimationExt, AnyElement, App, ElementId, IntoElement, ParentElement, Refineable, RenderOnce,
    Window, div, prelude::*, relative,
};
#[derive(IntoElement, Default)]
pub struct SegmentedControl {
    segments: Vec<AnyElement>,
    id: Option<ElementId>,
    selected: Option<usize>,
    compact: bool,
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
    pub fn compact(mut self) -> Self {
        self.compact = true;
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
        let settings = crate::ui::primitives::control_scope::current().settings;
        let root = div()
            .w_full()
            .when(self.compact, |d| d.max_w(gpui::px(300.)))
            .min_w_0()
            .flex()
            .items_center()
            .gap(Spacing::Xxs.px())
            .p(Spacing::Xxs.px())
            .rounded(Radius::Large.px())
            .bg(Color::Accent.get().opacity(0.055))
            .when(settings, |d| {
                d.rounded(gpui::px(8.))
                    .border_0()
                    .bg(Color::RowSurface.get())
            });
        if let Some(id) = self.id {
            let count = self.segments.len();
            let root = root.id(id.clone()).w_full();
            let mut track = div().relative().w_full().flex().items_center();
            if let Some(selected) = self.selected.filter(|i| *i < count) {
                let fraction = 1. / count as f32;
                let debug = format!("segment-indicator-{id}");
                let mut indicator = div()
                    .id("selection-surface")
                    .absolute()
                    .left_0()
                    .top_0()
                    .h_full()
                    .w(relative(fraction))
                    .rounded(Radius::Medium.px())
                    .border_1()
                    .border_color(Color::DefaultBorder.get())
                    .bg(Color::ElevatedSurface.get())
                    .when(settings, |d| {
                        controls::raised(d, Role::Selection, Phase::Selected, 0., 0.)
                            .rounded(gpui::px(9.))
                    });
                let mut style = gpui::Style::default();
                style.refine(indicator.style());
                track = track.child(
                    indicator
                        .bg(gpui::rgba(0))
                        .border_0()
                        .shadow(vec![])
                        .with_spring(
                            "selection-position",
                            motion::selection(selected as f32),
                            move |d, position| {
                                d.child(
                                    gpui::canvas(
                                        |_, _, _| {},
                                        move |mut bounds, _, window, _| {
                                            bounds.origin.x += bounds.size.width
                                                * position
                                                    .clamp(0., count.saturating_sub(1) as f32);
                                            motion_surface::paint(&debug, bounds, &style, window);
                                        },
                                    )
                                    .size_full(),
                                )
                            },
                        ),
                );
            }
            crate::ui::primitives::control_scope::ControlScope::new(
                crate::ui::primitives::control_scope::ControlStyle {
                    segment_indicator: self.selected.is_some_and(|i| i < count),
                    ..crate::ui::primitives::control_scope::current()
                },
                root.child(
                    track.children(
                        self.segments
                            .into_iter()
                            .map(|segment| div().relative().flex_1().min_w_0().child(segment)),
                    ),
                ),
            )
            .into_any_element()
        } else {
            root.children(
                self.segments
                    .into_iter()
                    .map(|segment| div().flex_1().min_w_0().child(segment)),
            )
            .into_any_element()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::primitives::{Button, ButtonSize};
    use gpui::{Context, Render, TestAppContext, px};
    use std::time::Duration;
    struct Fixture {
        selected: usize,
    }
    impl Render for Fixture {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w(px(300.)).child(
                SegmentedControl::new()
                    .id("interaction-segments")
                    .selected(self.selected)
                    .children(
                        ["Easy", "Advanced"]
                            .into_iter()
                            .enumerate()
                            .map(|(i, label)| {
                                Button::new(format!("interaction-segment-{i}"), label)
                                    .segment()
                                    .size(ButtonSize::Compact)
                                    .selected(self.selected == i)
                            }),
                    ),
            )
        }
    }
    #[gpui::test]
    fn interaction_segment_geometry_reverses_and_respects_reduced_motion(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            cx.set_reduce_motion(true);
            Fixture { selected: 0 }
        });
        cx.run_until_parked();
        let first = cx.debug_bounds("interaction-segment-0").unwrap();
        let second = cx.debug_bounds("interaction-segment-1").unwrap();
        assert!(first.right() <= second.left());
        assert!((first.size.width - second.size.width).abs() <= px(1.));
        let off = motion_surface::test_bounds("segment-indicator-interaction-segments");
        assert!((off.left() - first.left()).abs() <= px(1.));
        cx.update(|_, cx| {
            cx.set_reduce_motion(false);
            view.update(cx, |v, cx| {
                v.selected = 1;
                cx.notify();
            });
        });
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(40));
        cx.update(|w, cx| w.simulate_next_frame(cx));
        cx.run_until_parked();
        let moving = motion_surface::test_bounds("segment-indicator-interaction-segments");
        assert!(moving.left() > first.left() && moving.left() < second.left());
        cx.update(|_, cx| {
            view.update(cx, |v, cx| {
                v.selected = 0;
                cx.notify();
            })
        });
        cx.run_until_parked();
        let reverse = motion_surface::test_bounds("segment-indicator-interaction-segments");
        assert!(
            (reverse.left() - moving.left()).abs() < px(12.),
            "reversal reset position"
        );
        cx.update(|_, cx| {
            cx.set_reduce_motion(true);
            view.update(cx, |v, cx| {
                v.selected = 1;
                cx.notify();
            });
        });
        cx.run_until_parked();
        let on = motion_surface::test_bounds("segment-indicator-interaction-segments");
        assert!((on.left() - second.left()).abs() <= px(1.));
        cx.update(|w, cx| w.simulate_next_frame(cx));
        cx.run_until_parked();
        assert_eq!(cx.update(|w, cx| w.simulate_next_frame(cx)), 0);
        assert_eq!(cx.debug_bounds("interaction-segment-0").unwrap(), first);
        assert_eq!(cx.debug_bounds("interaction-segment-1").unwrap(), second);
    }
}
