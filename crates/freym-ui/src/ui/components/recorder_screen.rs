use crate::ui::theme::{colors::Color, typography::FONT_FAMILY};
use gpui::{AnyElement, App, IntoElement, ParentElement, RenderOnce, Window, div, prelude::*, px};

/// Logical-pixel port of recorder-layout.css. Preview and Audio share the wide row.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RecorderGeometry {
    pub width: f32,
    pub left: f32,
    pub right: f32,
    pub gap: f32,
    pub media: f32,
    pub audio: f32,
    pub row_gap: f32,
}
impl RecorderGeometry {
    pub(crate) fn new(width: f32, height: f32) -> Self {
        if width >= 1100. {
            let inset = (width * 0.04).clamp(40., 80.);
            let budget = (height - 338.).max(160.);
            let workspace = 1720_f32
                .min(width - inset * 2.)
                .min((budget * 16. / 9. * 2.55 / 1.55 + 28.).max(920.));
            let gap = (workspace * 0.016).clamp(20., 28.);
            let dock = if width >= 1400. { 452. } else { 398. };
            let left = (((workspace - gap) * 1.55 / 2.55)
                + ((workspace - 1045.) * 0.28).clamp(0., 144.))
            .min(workspace - gap - dock)
            .min((budget * 16. / 9.).max(240.));
            Self {
                width: workspace,
                left,
                right: workspace - gap - left,
                gap,
                media: left * 9. / 16.,
                audio: left * 9. / 16.,
                row_gap: 24.,
            }
        } else {
            let workspace = (width - 48.).max(640.);
            let right = 380.;
            let gap = 24.;
            let left = (workspace - right - gap).max(240.);
            Self {
                width: workspace,
                left,
                right,
                gap,
                media: left * 9. / 16.,
                audio: (height - 206.).clamp(260., 434.),
                row_gap: 16.,
            }
        }
    }
}
pub(super) fn left_width(window: &Window) -> f32 {
    RecorderGeometry::new(
        f32::from(window.viewport_size().width),
        f32::from(window.viewport_size().height),
    )
    .left
}

#[derive(IntoElement)]
pub struct RecorderScreen {
    titlebar: AnyElement,
    footer: AnyElement,
    media: Option<(AnyElement, AnyElement)>,
    cards: Vec<AnyElement>,
    sections: Vec<AnyElement>,
    overlays: Vec<AnyElement>,
    messages: Option<AnyElement>,
    native_viewport: Option<gpui::Bounds<gpui::Pixels>>,
    preview_workbench: bool,
    bottom_reserve: f32,
}
impl RecorderScreen {
    pub fn new(titlebar: impl IntoElement, footer: impl IntoElement) -> Self {
        Self {
            titlebar: titlebar.into_any_element(),
            footer: footer.into_any_element(),
            media: None,
            cards: vec![],
            sections: vec![],
            overlays: vec![],
            messages: None,
            native_viewport: None,
            preview_workbench: false,
            bottom_reserve: 0.,
        }
    }
    pub fn artwork(self, _: bool) -> Self {
        self
    }
    pub fn preview_workbench(mut self, value: bool) -> Self {
        self.preview_workbench = value;
        self
    }
    pub fn bottom_reserve(mut self, value: f32) -> Self {
        self.bottom_reserve = value;
        self
    }
    pub fn native_viewport(mut self, viewport: Option<gpui::Bounds<gpui::Pixels>>) -> Self {
        self.native_viewport = viewport;
        self
    }
    pub fn media(mut self, capture: impl IntoElement, audio: impl IntoElement) -> Self {
        self.media = Some((capture.into_any_element(), audio.into_any_element()));
        self
    }
    pub fn cards(mut self, cards: impl IntoIterator<Item = AnyElement>) -> Self {
        self.cards.extend(cards);
        self
    }
    pub fn overlay(mut self, overlay: impl IntoElement) -> Self {
        self.overlays.push(overlay.into_any_element());
        self
    }
    pub fn messages(mut self, messages: impl IntoElement) -> Self {
        self.messages = Some(messages.into_any_element());
        self
    }
}
impl ParentElement for RecorderScreen {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.sections.extend(elements);
    }
}
impl RenderOnce for RecorderScreen {
    fn render(self, window: &mut Window, _: &mut App) -> impl IntoElement {
        let (width, height) = (
            f32::from(window.viewport_size().width),
            f32::from(window.viewport_size().height),
        );
        let geometry = RecorderGeometry::new(
            width,
            height - if self.preview_workbench { 100. } else { 0. } - self.bottom_reserve,
        );
        let mut root = div()
            .id("recorder-shell")
            .relative()
            .size_full()
            .overflow_hidden()
            .font_family(FONT_FAMILY)
            .text_size(px(14.))
            .line_height(px(20.3))
            .text_color(Color::PrimaryText.get());
        // Preserve the independent child HWND's transparent hole; no bitmap backdrop.
        let rectangles = if let Some(b) = self.native_viewport {
            let (x, y, w, h) = (
                f32::from(b.left()),
                f32::from(b.top()),
                f32::from(b.size.width),
                f32::from(b.size.height),
            );
            vec![
                (0., 0., width, y),
                (0., y + h, width, (height - y - h).max(0.)),
                (0., y, x, h),
                (x + w, y, (width - x - w).max(0.), h),
            ]
        } else {
            vec![(0., 0., width, height)]
        };
        for (x, y, w, h) in rectangles {
            root = root.child(
                div()
                    .absolute()
                    .left(px(x))
                    .top(px(y))
                    .w(px(w))
                    .h(px(h))
                    .bg(Color::Background.get()),
            );
        }
        let mut workspace = div()
            .id("recorder-media")
            .debug_selector(|| "recorder-media".into())
            .w(px(geometry.width))
            .flex()
            .gap(px(geometry.gap));
        if let Some((capture, audio)) = self.media {
            workspace = workspace
                .child(
                    div()
                        .w(px(geometry.left))
                        .flex_shrink_0()
                        .flex()
                        .flex_col()
                        .gap(px(geometry.row_gap))
                        .child(
                            div()
                                .id("recorder-picture")
                                .debug_selector(|| "recorder-picture".into())
                                .h(px(geometry.media))
                                .flex_shrink_0()
                                .child(capture),
                        )
                        .child(
                            div()
                                .id("recorder-setting-cards")
                                .flex()
                                .gap(px(12.))
                                .children(self.cards),
                        ),
                )
                .child(
                    div()
                        .w(px(geometry.right))
                        .flex_shrink_0()
                        .flex()
                        .flex_col()
                        .gap(px(geometry.row_gap))
                        .child(div().h(px(geometry.audio)).flex_shrink_0().child(audio))
                        .children(self.sections)
                        .child(self.footer),
                );
        }
        root.child(
            div()
                .relative()
                .size_full()
                .flex()
                .flex_col()
                .child(self.titlebar)
                .child(
                    div()
                        .id("recorder-content")
                        .flex_1()
                        .min_h_0()
                        .pt(px(24.))
                        .flex()
                        .items_start()
                        .justify_center()
                        .child(workspace),
                ),
        )
        .when_some(self.messages, |d, messages| {
            d.child(
                div()
                    .id("recorder-messages")
                    .absolute()
                    .bottom(px(8.))
                    .left(px(24.))
                    .right(px(24.))
                    .max_h(px(80.))
                    .overflow_y_scroll()
                    .child(messages),
            )
        })
        .children(self.overlays)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ui_lab_wide_geometry_matches_measured_browser_bounds() {
        for (w, h, expected_left, expected_right, expected_height) in [
            (1280., 720., 679.1042, 446.1354, 381.9961),
            (1600., 900., 996.448, 452., 560.502),
        ] {
            let g = RecorderGeometry::new(w, h);
            assert!((g.left - expected_left).abs() < 0.1, "{g:?}");
            assert!((g.right - expected_right).abs() < 0.1, "{g:?}");
            assert!((g.media - expected_height).abs() < 0.1, "{g:?}");
            assert_eq!(g.media, g.audio);
        }
    }
}
