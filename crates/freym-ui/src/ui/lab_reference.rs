//! UI Lab material/geometry study. Opt-in workbench only, no media/service calls.
pub mod geometry;

use gpui::{
    App, BoxShadow, Context, Div, ElementId, FontWeight, IntoElement, Refineable, Render, Stateful,
    Window, div, linear_color_stop, linear_gradient, prelude::*, px, rgb, rgba,
};
use std::borrow::Cow;

const FONT: &str = "Freym Lab Sans";

/// Register the same outlines as UI Lab without changing installed system fonts.
pub fn install_fonts(cx: &App) -> anyhow::Result<()> {
    cx.text_system().add_fonts(vec![
        Cow::Borrowed(include_bytes!(
            "../../assets/fonts/ui-lab/FreymLabSans-400.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../../assets/fonts/ui-lab/FreymLabSans-500.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../../assets/fonts/ui-lab/FreymLabSans-600.ttf"
        )),
    ])
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Secondary,
    Primary,
    Selection,
    Quiet,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Rest,
    Hover,
    Pressed,
    Selected,
    Disabled,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum ContextStyle {
    Recorder,
    Settings,
}

fn shadow(y: f32, blur: f32, color: u32, inset: bool) -> BoxShadow {
    let shadow = BoxShadow::new(px(0.), px(y), rgba(color).into()).blur_radius(px(blur));
    if inset { shadow.inset() } else { shadow }
}

// Top-to-bottom stops from amoled-controls.css, not an approximation via solids.
fn stops(kind: Kind, state: State) -> (u32, u32) {
    match (kind, state) {
        (_, State::Disabled) => (0x1b1b1e, 0x171719),
        (Kind::Quiet, _) => (0, 0),
        (Kind::Primary, State::Hover) => (0xfffefb, 0xeeedea),
        (Kind::Primary, State::Pressed) => (0xd7d6d3, 0xe4e3df),
        (Kind::Primary, _) => (0xf5f4f1, 0xe3e2df),
        (_, State::Pressed) => (0x1b1b1e, 0x232326),
        (Kind::Selection, State::Selected) => (0x303034, 0x252529),
        (Kind::Selection, State::Hover) => (0x2c2c30, 0x222225),
        (Kind::Selection, _) => (0x252528, 0x1b1b1e),
        (_, State::Hover) => (0x323235, 0x27272a),
        _ => (0x2a2a2d, 0x202023),
    }
}

fn shadows(kind: Kind, state: State) -> Vec<BoxShadow> {
    match (kind, state) {
        (Kind::Quiet, _) => vec![],
        (_, State::Disabled) => vec![
            shadow(1., 0., 0xffffff07, true),
            shadow(1., 2., 0x00000030, false),
        ],
        (Kind::Primary, State::Pressed) => vec![
            shadow(2., 4., 0x00000024, true),
            shadow(1., 0., 0xffffff40, true),
            shadow(1., 2., 0x00000060, false),
        ],
        (Kind::Primary, State::Hover) => vec![
            shadow(1., 0., 0xffffffff, true),
            shadow(-1., 0., 0xb6b5b266, true),
            shadow(3., 6., 0x00000070, false),
            shadow(0., 14., 0xffffff12, false),
        ],
        (Kind::Primary, _) => vec![
            shadow(1., 0., 0xffffffef, true),
            shadow(-1., 0., 0xb6b5b273, true),
            shadow(2., 4., 0x00000070, false),
            shadow(0., 12., 0xffffff0d, false),
        ],
        (_, State::Pressed) => vec![
            shadow(2., 3., 0x00000055, true),
            shadow(1., 0., 0xffffff04, true),
            shadow(1., 2., 0x00000050, false),
        ],
        (_, State::Hover) => vec![
            shadow(1., 0., 0xffffff1a, true),
            shadow(-1., 0., 0x00000060, true),
            shadow(2., 4., 0x00000080, false),
            shadow(6., 12., 0x00000038, false),
        ],
        _ => vec![
            shadow(1., 0., 0xffffff12, true),
            shadow(-1., 0., 0x00000060, true),
            shadow(2., 3., 0x00000070, false),
            shadow(5., 10., 0x00000030, false),
        ],
    }
}

fn face(kind: Kind, state: State, context: ContextStyle, low_cost: bool) -> gpui::StyleRefinement {
    let mut style = gpui::StyleRefinement::default();
    // Use the same Styled API for rest/hover/pressed to keep recipes coherent.
    if context == ContextStyle::Settings {
        style = style.bg(rgb(
            if matches!(state, State::Hover | State::Pressed | State::Selected) {
                0x242424
            } else {
                0x101010
            },
        ));
    } else if kind == Kind::Quiet {
        style = style.bg(rgba(match state {
            State::Hover => 0xffffff09,
            State::Pressed => 0xffffff08,
            _ => 0x00000000,
        }));
    } else {
        let (top, bottom) = stops(kind, state);
        style = style.bg(linear_gradient(
            180.,
            linear_color_stop(rgb(top), 0.),
            linear_color_stop(rgb(bottom), 1.),
        ));
        if !low_cost {
            style = style.shadow(shadows(kind, state));
        }
    }
    style.text_color(rgb(if state == State::Disabled {
        0x777980
    } else if kind == Kind::Primary && context == ContextStyle::Recorder {
        0x202124
    } else {
        0xeeeff2
    }))
}

fn apply_face<T: Styled>(
    mut element: T,
    kind: Kind,
    state: State,
    context: ContextStyle,
    low_cost: bool,
) -> T {
    element
        .style()
        .refine(&face(kind, state, context, low_cost));
    element
}

fn control(
    id: impl Into<ElementId>,
    label: &str,
    kind: Kind,
    state: State,
    context: ContextStyle,
    low_cost: bool,
) -> Stateful<Div> {
    let disabled = state == State::Disabled;
    div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label.to_owned())
        .aria_selected(state == State::Selected)
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .font_family(FONT)
        .font_weight(FontWeight::MEDIUM)
        .text_size(px(if context == ContextStyle::Settings {
            12.
        } else {
            13.
        }))
        .line_height(px(if context == ContextStyle::Settings {
            17.4
        } else {
            18.85
        }))
        .h(px(if context == ContextStyle::Settings {
            30.
        } else {
            36.
        }))
        .px(px(if context == ContextStyle::Settings {
            11.
        } else {
            12.
        }))
        .rounded(px(8.))
        .border_1()
        .border_color(if context == ContextStyle::Settings {
            rgba(0x2c2c2cff)
        } else {
            rgba(0x00000000)
        })
        .map(|s| apply_face(s, kind, state, context, low_cost))
        .opacity(if disabled { 0.55 } else { 1. })
        .when(!disabled, |d| {
            d.focusable()
                .tab_index(0)
                .cursor_pointer()
                .hover(move |s| apply_face(s, kind, State::Hover, context, low_cost))
                .active(move |s| apply_face(s, kind, State::Pressed, context, low_cost))
                .focus_visible(|s| s.border_color(rgb(0xd9dce5)))
        })
        .child(label.to_owned())
}

fn switch(checked: bool, low_cost: bool) -> Div {
    let (top, bottom, thumb_top, thumb_bottom) = if checked {
        (0xdedee2, 0xbcbec5, 0x42434a, 0x27282f)
    } else {
        (0x202023, 0x29292d, 0xa8aab0, 0x85878e)
    };
    div()
        .w(px(28.))
        .h(px(16.))
        .rounded_full()
        .flex_shrink_0()
        .bg(linear_gradient(
            180.,
            linear_color_stop(rgb(top), 0.),
            linear_color_stop(rgb(bottom), 1.),
        ))
        .when(!low_cost, |d| {
            d.shadow(vec![
                shadow(1., 3., 0x00000070, true),
                shadow(-1., 0., 0xffffff09, true),
            ])
        })
        .child(
            div()
                .size(px(12.))
                .mt(px(2.))
                .ml(px(if checked { 14. } else { 2. }))
                .rounded_full()
                .bg(linear_gradient(
                    180.,
                    linear_color_stop(rgb(thumb_top), 0.),
                    linear_color_stop(rgb(thumb_bottom), 1.),
                ))
                .when(!low_cost, |d| {
                    d.shadow(vec![
                        shadow(1., 0., 0xffffff33, true),
                        shadow(1., 2., 0x00000070, false),
                    ])
                }),
        )
}

#[derive(Default)]
pub struct ReferenceSheet {
    clicks: usize,
    checked: bool,
    low_cost: bool,
}
impl Render for ReferenceSheet {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let low_cost = self.low_cost;
        let row = || div().flex().items_center().gap(px(12.)).flex_wrap();
        let caption = |s: &str| {
            div()
                .text_size(px(12.))
                .text_color(rgb(0xb5b5b5))
                .child(s.to_owned())
        };
        div().id("lab-reference").size_full().overflow_y_scroll().p(px(32.))
            .font_family(FONT).text_size(px(14.)).line_height(px(20.3)).text_color(rgb(0xf5f5f5)).bg(rgb(0x000000))
            .child(div().flex().flex_col().gap(px(20.))
                .child("UI Lab · GPUI component study · AMOLED / filled")
                .child(caption("Material specimens. Motion, menus and full screen parity remain separate acceptance items."))
                .child(row().child(control("lab-secondary", "Secondary", Kind::Secondary, State::Rest, ContextStyle::Recorder, low_cost)
                    .debug_selector(|| "lab-secondary".into())
                    .on_click(cx.listener(|this, _, _, cx| { this.clicks += 1; cx.notify(); })))
                    .child(control("lab-primary", "Primary", Kind::Primary, State::Rest, ContextStyle::Recorder, low_cost))
                    .child(control("lab-selected", "Selected", Kind::Selection, State::Selected, ContextStyle::Recorder, low_cost))
                    .child(control("lab-quiet", "Quiet", Kind::Quiet, State::Rest, ContextStyle::Recorder, low_cost))
                    .child(control("lab-disabled", "Disabled", Kind::Secondary, State::Disabled, ContextStyle::Recorder, low_cost)
                        .debug_selector(|| "lab-disabled".into())))
                .child(row().children([State::Rest, State::Hover, State::Pressed, State::Disabled].into_iter().enumerate().map(|(i, state)|
                    control(("lab-states", i), ["Rest", "Hover", "Pressed", "Disabled"][i], Kind::Secondary, state, ContextStyle::Recorder, low_cost))))
                .child(row().child(div().id("lab-switch").debug_selector(|| "lab-switch".into())
                    .role(gpui::Role::Switch).aria_label("Example switch").aria_toggled(if self.checked { gpui::Toggled::True } else { gpui::Toggled::False })
                    .focusable().tab_index(0).cursor_pointer().flex().items_center().gap(px(8.)).h(px(36.))
                    .child(switch(self.checked, low_cost)).child("Example switch")
                    .on_click(cx.listener(|this, _, _, cx| { this.checked = !this.checked; cx.notify(); })))
                    .child(control("lab-cost", if low_cost { "Full materials" } else { "Low cost materials" }, Kind::Secondary, State::Rest, ContextStyle::Recorder, low_cost)
                        .on_click(cx.listener(|this, _, _, cx| { this.low_cost = !this.low_cost; cx.notify(); }))))
                .child(caption("Settings uses flat 30 px / 12 px controls, even when Filled buttons is enabled."))
                .child(row().child(control("lab-settings", "Save settings version", Kind::Secondary, State::Rest, ContextStyle::Settings, low_cost)
                    .debug_selector(|| "lab-settings".into()))
                    .child(control("lab-settings-disabled", "Restore previous", Kind::Secondary, State::Disabled, ContextStyle::Settings, low_cost)))
                .child(caption(&format!("Example activations: {}", self.clicks))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext, size};
    #[gpui::test]
    fn lab_specimens_preserve_density_and_disabled_semantics(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| ReferenceSheet::default());
        cx.simulate_resize(size(px(1280.), px(850.)));
        cx.run_until_parked();
        assert_eq!(
            cx.debug_bounds("lab-secondary").unwrap().size.height,
            px(36.)
        );
        assert_eq!(
            cx.debug_bounds("lab-settings").unwrap().size.height,
            px(30.)
        );
        for id in ["lab-secondary", "lab-disabled", "lab-switch"] {
            let point = cx.debug_bounds(id).unwrap().center();
            cx.simulate_click(point, Modifiers::default());
            cx.run_until_parked();
        }
        cx.update(|_, cx| {
            let sheet = view.read(cx);
            assert_eq!(sheet.clicks, 1);
            assert!(sheet.checked);
        });
    }
}
