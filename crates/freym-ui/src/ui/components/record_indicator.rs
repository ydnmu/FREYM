use super::RecordButtonState;
use crate::ui::theme::{colors::Color, spacing::Layout};
use gpui::{
    App, Bounds, IntoElement, RenderOnce, Window, canvas, div, fill, point, prelude::*, px, size,
};

#[derive(IntoElement)]
pub(super) struct RecordIndicator {
    pub state: RecordButtonState,
    pub phase: f32,
}
impl RenderOnce for RecordIndicator {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let state = self.state;
        let phase = self.phase;
        div().flex_shrink_0().size(px(Layout::ICON_SMALL)).child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    let center = bounds.center();
                    let marker =
                        |w: f32, h: f32, radius: f32, color: Color, window: &mut Window| {
                            window.paint_quad(
                                fill(
                                    Bounds::new(
                                        point(center.x - px(w / 2.), center.y - px(h / 2.)),
                                        size(px(w), px(h)),
                                    ),
                                    color.get(),
                                )
                                .corner_radii(px(radius)),
                            );
                        };
                    match state {
                        RecordButtonState::Ready => marker(12., 12., 6., Color::Danger, window),
                        RecordButtonState::Recording => {
                            marker(11., 11., 3., Color::OnAccent, window)
                        }
                        RecordButtonState::Paused => {
                            for offset in [-3., 3.] {
                                window.paint_quad(
                                    fill(
                                        Bounds::new(
                                            point(center.x + px(offset - 1.5), center.y - px(5.)),
                                            size(px(3.), px(10.)),
                                        ),
                                        Color::Warning.get(),
                                    )
                                    .corner_radii(px(1.)),
                                );
                            }
                        }
                        RecordButtonState::Failed => marker(10., 10., 3., Color::Error, window),
                        RecordButtonState::Countdown | RecordButtonState::Stopping => {
                            for dot in 0..8 {
                                let angle = dot as f32 / 8. * std::f32::consts::TAU;
                                let alpha = 0.2 + ((dot as f32 / 8. - phase).rem_euclid(1.)) * 0.8;
                                window.paint_quad(
                                    fill(
                                        Bounds::new(
                                            point(
                                                center.x + px(angle.cos() * 5. - 1.),
                                                center.y + px(angle.sin() * 5. - 1.),
                                            ),
                                            size(px(2.), px(2.)),
                                        ),
                                        Color::Accent.get().opacity(alpha),
                                    )
                                    .corner_radii(px(1.)),
                                );
                            }
                        }
                    }
                },
            )
            .size_full(),
        )
    }
}
