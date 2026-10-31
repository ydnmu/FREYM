//! Pointer-driven light, retained only for the life of its keyed card.
use crate::ui::theme::{colors::Color, motion};
use gpui::{
    AnimationExt, Bounds, Context, Pixels, Point, Render, Window, canvas, div, fill, point,
    prelude::*, px, size,
};

#[derive(Default)]
pub(super) struct CardLight {
    position: Option<Point<Pixels>>,
    visible: bool,
}
impl CardLight {
    pub(super) fn move_to(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if !self.visible
            || self
                .position
                .is_none_or(|old| (old.x - position.x).abs() + (old.y - position.y).abs() >= px(2.))
        {
            self.position = Some(position);
            self.visible = true;
            cx.notify();
        }
    }
    pub(super) fn leave(&mut self, cx: &mut Context<Self>) {
        if self.visible {
            self.visible = false;
            cx.notify();
        }
    }
}
impl Render for CardLight {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
        let position = self.position;
        div()
            .size_full()
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        let Some(center) = position.filter(|p| bounds.contains(p)) else {
                            return;
                        };
                        // A bounded stack of translucent discs gives a smooth falloff without
                        // a shader, CPU image transport or a continuously running animation.
                        for layer in 0..motion::SPOTLIGHT_LAYERS {
                            let radius = motion::SPOTLIGHT_RADIUS
                                * (1. - layer as f32 / motion::SPOTLIGHT_LAYERS as f32);
                            let light = Bounds::new(
                                point(center.x - px(radius), center.y - px(radius)),
                                size(px(radius * 2.), px(radius * 2.)),
                            );
                            window.paint_quad(
                                fill(light, Color::Accent.get().opacity(0.008))
                                    .corner_radii(px(radius)),
                            );
                        }
                    },
                )
                .size_full(),
            )
            .with_spring(
                "light-visibility",
                motion::selection(if self.visible { 1. } else { 0. }),
                |d, alpha| d.opacity(alpha.clamp(0., 1.)),
            )
    }
}
