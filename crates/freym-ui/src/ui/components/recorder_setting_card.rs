use crate::ui::{
    primitives::{
        Button, ButtonSize, Icon, IconButton, IconSize, Icons, Text, TextVariant, clickable,
    },
    theme::{colors::Color, motion},
};
use gpui::{
    AnimationExt, AnyElement, App, ElementId, IntoElement, RenderOnce, SharedString, Window, div,
    prelude::*, px,
};
use std::rc::Rc;
type Dismiss = Rc<dyn Fn(&(), &mut Window, &mut App)>;

/// The edge (or center) of a fixed-size panel from which its mask opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevealOrigin {
    Left,
    Center,
    Right,
}
impl RevealOrigin {
    fn mask(self, width: f32, progress: f32) -> (f32, f32) {
        let revealed = width * progress.clamp(0., 1.);
        let left = match self {
            Self::Left => 0.,
            Self::Center => (width - revealed) / 2.,
            Self::Right => width - revealed,
        };
        (left, revealed)
    }
}

// One spring drives both the mask and its inverse content offset. The content
// keeps its full layout width and its screen position through every frame.
#[derive(IntoElement)]
struct RevealedPanel {
    id: SharedString,
    body: AnyElement,
    width: f32,
    height: f32,
    origin: RevealOrigin,
    progress: f32,
    open: bool,
}
impl RenderOnce for RevealedPanel {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let (left, width) = self.origin.mask(self.width, self.progress);
        let body = crate::ui::primitives::control_scope::ControlScope::new(
            crate::ui::primitives::control_scope::ControlStyle {
                home: true,
                settings: false,
                inert: !self.open || self.progress < 0.95,
                segment_indicator: false,
            },
            div()
                .absolute()
                .top_0()
                .left(px(-left))
                .w(px(self.width))
                .h(px(self.height))
                .child(self.body),
        );
        div()
            .id(self.id.clone())
            .debug_selector(move || self.id.to_string())
            .relative()
            .left(px(left))
            .w(px(width))
            .h(px(self.height))
            .overflow_hidden()
            .opacity(if width > 0. { 1. } else { 0. })
            .child(body)
    }
}
/// Fixed trigger footprint. A full-width body is revealed below the row.
#[derive(IntoElement)]
pub struct RecorderSettingCard {
    button: Button,
    label: SharedString,
    detail: SharedString,
    icon: Icons,
    expanded: bool,
    controls: Option<AnyElement>,
    row_width: Option<f32>,
    origin: Option<(f32, f32)>,
    reveal_origin: RevealOrigin,
    dismiss: Option<Dismiss>,
    column: bool,
}
impl RecorderSettingCard {
    pub fn new(
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        detail: impl Into<SharedString>,
        icon: Icons,
    ) -> Self {
        let label = label.into();
        Self {
            button: Button::new(id, label.clone()).size(ButtonSize::Card),
            label,
            detail: detail.into(),
            icon,
            expanded: false,
            controls: None,
            row_width: None,
            origin: None,
            reveal_origin: match icon {
                Icons::Camera => RevealOrigin::Center,
                Icons::Quality => RevealOrigin::Right,
                _ => RevealOrigin::Left,
            },
            dismiss: None,
            column: false,
        }
    }
    pub fn row_width(mut self, width: f32) -> Self {
        self.row_width = Some(width);
        self
    }
    pub fn column(mut self) -> Self {
        self.column = true;
        self
    }
    pub fn origin(mut self, x: f32, y: f32) -> Self {
        self.origin = Some((x, y));
        self
    }
    pub fn reveal_origin(mut self, origin: RevealOrigin) -> Self {
        self.reveal_origin = origin;
        self
    }
    pub fn expanded(mut self, expanded: bool) -> Self {
        self.expanded = expanded;
        self
    }
    pub fn controls(mut self, controls: impl IntoElement) -> Self {
        self.controls = Some(controls.into_any_element());
        self
    }
    pub fn on_dismiss(mut self, handler: impl Fn(&(), &mut Window, &mut App) + 'static) -> Self {
        self.dismiss = Some(Rc::new(handler));
        self
    }
}
clickable!(RecorderSettingCard);
impl RenderOnce for RecorderSettingCard {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let row_width = self
            .row_width
            .unwrap_or_else(|| super::recorder_screen::left_width(window));
        let card_width = if self.column {
            row_width
        } else {
            (row_width - 24.) / 3.
        };
        let slot = match self.icon {
            Icons::Camera => 1.,
            Icons::Quality => 2.,
            _ => 0.,
        };
        let seed = if self.column {
            0.
        } else {
            slot * (card_width + 12.)
        };
        let section_id = format!("section-{}", self.button.id());

        let heading_size = if self.column { 14. } else { 17. };

        let controls_id = format!("{section_id}-controls");
        let heading = self.button.expanded(self.expanded).content(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .h(px(23.))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(Icon::new(self.icon))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(heading_size))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child(self.label.clone()),
                        )
                        .child(
                            Icon::new(Icons::Chevron)
                                .size(IconSize::Small)
                                .rotation(if self.expanded { 180. } else { 0. }),
                        ),
                )
                .child(
                    Text::new(self.detail)
                        .variant(TextVariant::Caption)
                        .color(Color::SecondaryText)
                        .truncate(),
                ),
        );
        let mut root = div()
            .id(section_id.clone())
            .relative()
            .w(px(card_width))
            .h(px(if self.column { 56. } else { 64. }))
            .flex_shrink_0()
            .child(heading);
        let g = super::recorder_screen::RecorderGeometry::new(
            f32::from(crate::ui::viewport(window, cx).width),
            f32::from(crate::ui::viewport(window, cx).height),
        );
        let (row_x, row_y) = self.origin.unwrap_or((
            (f32::from(crate::ui::viewport(window, cx).width) - g.width) / 2.,
            72. + g.media + g.row_gap,
        ));
        let panel_height = if self.column {
            160.
        } else {
            (f32::from(crate::ui::viewport(window, cx).height) - row_y - 76. - 20.).clamp(80., 160.)
        };
        let panel_top = if self.column {
            -panel_height - 12. - slot * 60.
        } else {
            76.
        };
        let interaction_top = if self.column {
            row_y - panel_height - 12.
        } else {
            row_y
        };
        let interaction_bottom = row_y
            + if self.column {
                176.
            } else {
                76. + panel_height
            };
        if self.expanded
            && let Some(dismiss) = self.dismiss.clone()
        {
            let key = dismiss.clone();
            root = root
                .on_key_down(move |event, window, cx| {
                    if event.keystroke.key == "escape" {
                        key(&(), window, cx);
                        cx.stop_propagation();
                    }
                })
                .on_mouse_down_out(move |event, window, cx| {
                    let x = f32::from(event.position.x);
                    let y = f32::from(event.position.y);
                    if x < row_x
                        || x > row_x + row_width
                        || y < interaction_top
                        || y > interaction_bottom
                    {
                        dismiss(&(), window, cx);
                    }
                });
        }
        let target = if self.expanded { 1. } else { 0. };
        let open = self.expanded;
        let mut body = div()
            .id(format!("{section_id}-body"))
            .debug_selector({
                let id = format!("{section_id}-body");
                move || id.clone()
            })
            .relative()
            .w(px(row_width))
            .h(px(panel_height))
            .overflow_y_scroll()
            .rounded(px(12.))
            .border_1()
            .border_color(Color::DefaultBorder.get())
            .bg(Color::PrimarySurface.get())
            .p(px(16.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .text_size(px(13.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(self.label.clone())
                    .when_some(self.dismiss, |d, dismiss| {
                        d.child(
                            IconButton::new(
                                format!("{section_id}-close"),
                                Icons::Close,
                                format!("Close {} settings", self.label),
                            )
                            .on_click(move |_, w, cx| dismiss(&(), w, cx)),
                        )
                    }),
            );
        if let Some(controls) = self.controls {
            body = body.child(controls);
        }
        let transition = motion::selection(target);

        let reveal = RevealedPanel {
            id: format!("{section_id}-mask").into(),
            body: body.into_any_element(),
            width: row_width,
            height: panel_height,
            origin: self.reveal_origin,
            progress: 0.,
            open,
        }
        .with_spring(
            format!("{section_id}-reveal"),
            transition.from(0.),
            |mut panel, progress| {
                panel.progress = progress.clamp(0., 1.);
                panel
            },
        );
        root = root.child(
            div()
                .id(controls_id)
                .absolute()
                .top(px(panel_top))
                .left(px(-seed))
                .w(px(row_width))
                .h(px(panel_height))
                .child(reveal),
        );
        root.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::RevealOrigin;

    #[test]
    fn directional_masks_keep_their_anchor_and_full_size_content_stationary() {
        for width in [288., 722., 1240.] {
            for progress in [0., 0.01, 0.25, 0.5, 0.85, 1.] {
                for origin in [
                    RevealOrigin::Left,
                    RevealOrigin::Center,
                    RevealOrigin::Right,
                ] {
                    let (left, revealed) = origin.mask(width, progress);
                    assert!(left >= 0. && left + revealed <= width + 0.001);
                    assert!((revealed - width * progress).abs() < 0.001);
                    match origin {
                        RevealOrigin::Left => assert_eq!(left, 0.),
                        RevealOrigin::Center => {
                            assert!((left + revealed / 2. - width / 2.).abs() < 0.001)
                        }
                        RevealOrigin::Right => assert!((left + revealed - width).abs() < 0.001),
                    }
                    // The inverse offset leaves a label at its original
                    // screen coordinate as the clipping edge passes over it.
                    let label_x = 24.;
                    assert!((left + (-left + label_x) - label_x).abs() < 0.001);
                }
            }
        }
    }
}
