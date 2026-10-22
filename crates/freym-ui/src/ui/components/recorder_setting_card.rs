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
use std::{cell::Cell, rc::Rc};
type Dismiss = Rc<dyn Fn(&(), &mut Window, &mut App)>;
#[derive(Default)]
struct RevealState {
    target: bool,
    from: f32,
    value: Rc<Cell<f32>>,
    generation: usize,
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
    dismiss: Option<Dismiss>,
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
            dismiss: None,
        }
    }
    pub fn row_width(mut self, width: f32) -> Self {
        self.row_width = Some(width);
        self
    }
    pub fn origin(mut self, x: f32, y: f32) -> Self {
        self.origin = Some((x, y));
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
        let card_width = (row_width - 24.) / 3.;
        let slot = match self.icon {
            Icons::Camera => 1.,
            Icons::Quality => 2.,
            _ => 0.,
        };
        let seed = slot * (card_width + 12.);
        let section_id = format!("section-{}", self.button.id());
        let controls_id = format!("{section_id}-controls");
        let state = window.use_keyed_state(format!("{section_id}-reveal"), cx, |_, _| {
            RevealState::default()
        });
        let (from, value, generation) = state.update(cx, |s, _| {
            if s.target != self.expanded {
                s.from = s.value.get();
                s.target = self.expanded;
                s.generation += 1;
            }
            (s.from, s.value.clone(), s.generation)
        });
        let heading = self.button.expanded(self.expanded).content(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .h(px(20.))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(Icon::new(self.icon))
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(14.))
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
                        .truncate(),
                ),
        );
        let mut root = div()
            .id(section_id.clone())
            .relative()
            .w(px(card_width))
            .h(px(54.))
            .flex_shrink_0()
            .child(heading);
        let g = super::recorder_screen::RecorderGeometry::new(
            f32::from(window.viewport_size().width),
            f32::from(window.viewport_size().height),
        );
        let (row_x, row_y) = self.origin.unwrap_or((
            (f32::from(window.viewport_size().width) - g.width) / 2.,
            72. + g.media + g.row_gap,
        ));
        let panel_height =
            (f32::from(window.viewport_size().height) - row_y - 66. - 16.).clamp(80., 160.);
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
                        || y < row_y
                        || y > row_y + 66. + panel_height
                    {
                        dismiss(&(), window, cx);
                    }
                });
        }
        if !self.expanded && value.get() <= 0.0001 {
            return root.into_any_element();
        }
        let target = if self.expanded { 1. } else { 0. };
        let open = self.expanded;
        let mut body = div()
            .absolute()
            .top_0()
            .w(px(row_width))
            .min_h(px(panel_height))
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
        let body = body.with_animation(
            format!("body-{generation}"),
            motion::expansion(open),
            move |d, t| {
                let progress = from + (target - from) * t;
                d.left(px(-seed * (1. - progress)))
            },
        );
        let scope = crate::ui::primitives::control_scope::ControlScope::new(
            crate::ui::primitives::control_scope::ControlStyle {
                settings: false,
                inert: !open,
            },
            body,
        );
        let reveal = div()
            .relative()
            .overflow_hidden()
            .rounded(px(10.))
            .border_1()
            .border_color(Color::DefaultBorder.get())
            .bg(Color::PrimarySurface.get())
            .child(scope)
            .with_animation(
                format!("face-{generation}"),
                motion::expansion(open),
                move |d, t| {
                    let progress = (from + (target - from) * t).clamp(0., 1.);
                    value.set(progress);
                    d.left(px(seed * (1. - progress)))
                        .w(px(card_width + (row_width - card_width) * progress))
                        .h(px(2. + (panel_height - 2.) * progress))
                },
            );
        root = root.child(
            div()
                .id(controls_id)
                .absolute()
                .top(px(66.))
                .left(px(-seed))
                .w(px(row_width))
                .h(px(panel_height))
                .child(reveal),
        );
        root.into_any_element()
    }
}
