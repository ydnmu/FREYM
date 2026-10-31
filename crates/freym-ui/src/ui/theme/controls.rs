//! Shared UI Lab control materials. Screen density is supplied by ControlScope.
use super::{ThemeId, colors::Color};
use gpui::{
    Background, BoxShadow, Hsla, Refineable, Rgba, StyleRefinement, Styled, linear_color_stop,
    linear_gradient, px, rgb, rgba,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Secondary,
    Primary,
    Selection,
    Quiet,
    Record,
    Danger,
    Keycap,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Rest,
    Hover,
    Pressed,
    Selected,
    Disabled,
}
fn gradient(top: Hsla, bottom: Hsla) -> Background {
    linear_gradient(
        180.,
        linear_color_stop(top, 0.),
        linear_color_stop(bottom, 1.),
    )
}
fn mix(color: Hsla, other: Hsla, fraction: f32) -> Hsla {
    let a: Rgba = color.into();
    let b: Rgba = other.into();
    Rgba {
        r: a.r * (1. - fraction) + b.r * fraction,
        g: a.g * (1. - fraction) + b.g * fraction,
        b: a.b * (1. - fraction) + b.b * fraction,
        a: a.a,
    }
    .into()
}
fn tint(hex: u32, alpha: f32) -> Hsla {
    rgba((hex << 8) | (alpha.clamp(0., 1.) * 255.) as u32).into()
}
fn white(alpha: f32) -> Hsla {
    tint(0xffffff, alpha)
}
fn black(alpha: f32) -> Hsla {
    tint(0x000000, alpha)
}
fn inset(y: f32, blur: f32, color: Hsla) -> BoxShadow {
    BoxShadow::new(px(0.), px(y), color)
        .blur_radius(px(blur))
        .inset()
}
fn cast(y: f32, blur: f32, spread: f32, color: Hsla) -> BoxShadow {
    BoxShadow::new(px(0.), px(y), color)
        .blur_radius(px(blur))
        .spread_radius(px(spread))
}
fn light_face(top: Hsla) -> bool {
    let color: Rgba = top.into();
    0.2126 * color.r + 0.7152 * color.g + 0.0722 * color.b > 0.55
}
/// One raised control material: a lit top edge, a quiet lower lip and, where a
/// real surface exists, a short contact shadow. Pressing only changes the fill;
/// elevation stays fixed so the control does not appear to sink.
fn elevation(top: Hsla, hover: f32, disabled: bool, casts: bool) -> Vec<BoxShadow> {
    if super::low_cost_effects() {
        return vec![];
    }
    let hover = hover.clamp(0., 1.);
    if disabled {
        return vec![inset(2., 1., white(0.03)), inset(-2., 1., white(0.012))];
    }
    let classic_light = ThemeId::current() == ThemeId::White;
    let mut shadows = if classic_light {
        vec![
            inset(2., 0., white(0.52 + 0.10 * hover)),
            inset(-2., 0., black(0.055 + 0.02 * hover)),
        ]
    } else if light_face(top) {
        // A light key needs a darker seam, not a brighter rim.
        vec![
            inset(2., 0., white(0.06 + 0.03 * hover)),
            inset(-2., 0., black(0.075 + 0.02 * hover)),
        ]
    } else {
        vec![
            inset(1., 0., white(0.08 + 0.04 * hover)),
            inset(-1., 0., black(0.20)),
        ]
    };
    if casts {
        let contact = if classic_light { 0.05 } else { 0.30 };
        let ambient = if classic_light { 0.025 } else { 0.16 };
        shadows.push(cast(3., 3. + 3. * hover, 0., black(contact)));
        shadows.push(cast(4. + 3. * hover, 10. + 6. * hover, -2., black(ambient)));
    }
    shadows
}
/// A lit hairline that the inset rim can read against without a hard outline.
fn edge(phase: Phase, hover: f32, press: f32) -> Hsla {
    let selected = if phase == Phase::Selected { 0.12 } else { 0. };
    let scope = crate::ui::primitives::control_scope::current();
    let rest = if scope.home || scope.settings {
        0.13
    } else {
        0.085
    };
    let alpha = rest + 0.035 * hover.clamp(0., 1.) + selected + 0.05 * press.clamp(0., 1.);
    white(alpha.clamp(0.02, 0.6))
}
/// Fills for the requested role, blended continuously between rest, hover and
/// press. Non-AMOLED palettes keep their exact tokens; the material only
/// changes their shape.
fn face(role: Role, phase: Phase, hover: f32, press: f32) -> (Hsla, Hsla) {
    if ThemeId::current() == ThemeId::Black {
        if phase == Phase::Disabled {
            return (rgb(0x232529).into(), rgb(0x1d1f23).into());
        }
        let selected = phase == Phase::Selected;
        let scope = crate::ui::primitives::control_scope::current();
        let home = scope.home || scope.settings;
        let (rest, over, down) = match role {
            Role::Record => (
                (0xe34b53, 0xc83440),
                (0xf45d64, 0xdc424e),
                (0xff7279, 0xef5360),
            ),
            Role::Primary => (
                (0xf7f7f8, 0xe7e9ed),
                (0xffffff, 0xf3f5f8),
                (0xffffff, 0xffffff),
            ),
            _ if home && selected => (
                (0x2e2e30, 0x202022),
                (0x363638, 0x28282a),
                (0x404043, 0x303033),
            ),
            _ if home => (
                (0x272729, 0x19191b),
                (0x303032, 0x222224),
                (0x3b3b3e, 0x2b2b2e),
            ),
            _ if selected => (
                (0x33363c, 0x2b2e34),
                (0x3b3f46, 0x32363d),
                (0x474b53, 0x3b3f47),
            ),
            _ => (
                (0x2d2f34, 0x26282d),
                (0x35383e, 0x2d3036),
                (0x42464e, 0x363a42),
            ),
        };
        let blend = |rest: u32, over: u32, down: u32| {
            mix(
                mix(rgb(rest).into(), rgb(over).into(), hover.clamp(0., 1.)),
                rgb(down).into(),
                press.clamp(0., 1.),
            )
        };
        return (blend(rest.0, over.0, down.0), blend(rest.1, over.1, down.1));
    }
    let primary = role == Role::Primary;
    if primary {
        let base = rgb(0xeeeff2).into();
        let highlight = (0.035 * hover + 0.10 * press).clamp(0., 1.);
        let base = mix(base, rgb(0xffffff).into(), highlight);
        (
            mix(base, rgb(0xffffff).into(), 0.08),
            mix(base, rgb(0x000000).into(), 0.06),
        )
    } else {
        let base = if phase == Phase::Selected {
            Color::HoverSurface.get()
        } else {
            Color::ElevatedSurface.get()
        };
        let over = mix(base, Color::HoverSurface.get(), hover.clamp(0., 1.));
        let down = mix(over, rgb(0xffffff).into(), 0.10 * press.clamp(0., 1.));
        (mix(down, rgb(0xffffff).into(), 0.07), down)
    }
}
fn surface<T: Styled>(
    mut element: T,
    role: Role,
    phase: Phase,
    hover: f32,
    press: f32,
    flat: bool,
    casts: bool,
) -> T {
    if crate::ui::primitives::control_scope::current().settings {
        return settings_control(element, role, phase);
    }
    let mut style = StyleRefinement::default();
    let base = match phase {
        Phase::Hover | Phase::Selected => Color::HoverSurface.get(),
        Phase::Pressed => Color::PressedSurface.get(),
        _ => Color::ElevatedSurface.get(),
    };
    let quiet = role == Role::Quiet;
    let filled = super::filled_buttons() && !flat;
    let primary = role == Role::Primary;
    let disabled = phase == Phase::Disabled;
    let mut ink = if disabled {
        Color::MutedText.get()
    } else {
        Color::PrimaryText.get()
    };
    if crate::ui::primitives::control_scope::current().home && !flat && !quiet {
        return frosted_bar(element, role, phase, hover, press);
    }
    if !filled {
        let background = if !flat {
            if phase == Phase::Rest || disabled {
                base.opacity(0.)
            } else if phase == Phase::Selected {
                Color::PrimaryText.get().opacity(0.08)
            } else {
                Color::PrimaryText
                    .get()
                    .opacity(0.04 * hover + 0.04 * press)
            }
        } else if quiet && phase == Phase::Rest {
            base.opacity(0.)
        } else if phase == Phase::Rest {
            Color::PrimarySurface.get()
        } else {
            base
        };
        style = style.bg(background).border_0().shadow(vec![]);
        if !flat && role == Role::Record && !disabled {
            ink = Color::Record.get();
        }
    } else if quiet {
        style = style
            .bg(gradient(
                Color::PrimaryText
                    .get()
                    .opacity(0.05 * hover.clamp(0., 1.) + 0.035 * press.clamp(0., 1.)),
                Color::PrimaryText
                    .get()
                    .opacity(0.02 * hover.clamp(0., 1.) + 0.03 * press.clamp(0., 1.)),
            ))
            .shadow(vec![]);
    } else {
        let (top, bottom) = face(role, phase, hover, press);
        style = style
            .bg(gradient(top, bottom))
            .shadow(elevation(top, hover, disabled, casts));
        if ThemeId::current() == ThemeId::Black {
            style = style.border_color(edge(phase, hover, press));
        }
        if !disabled && role == Role::Record {
            ink = rgb(0xffffff).into();
        } else if !disabled && primary {
            ink = if light_face(top) {
                rgb(0x202124).into()
            } else {
                Color::PrimaryText.get()
            };
        }
    }
    if role == Role::Danger && !disabled {
        ink = Color::Error.get();
    }
    element.style().refine(&style.text_color(ink));
    element
}
pub(crate) fn apply<T: Styled>(element: T, role: Role, phase: Phase, flat: bool) -> T {
    let hover = if phase == Phase::Hover { 1. } else { 0. };
    let press = if phase == Phase::Pressed { 1. } else { 0. };
    surface(element, role, phase, hover, press, flat, true)
}

/// Settings use the Sources row fill once, in either appearance. Interaction
/// states are authored surfaces, independent of Filled and animation progress.
pub(crate) fn settings_control<T: Styled>(element: T, role: Role, phase: Phase) -> T {
    if role == Role::Keycap {
        return element
            .bg(rgb(match phase {
                Phase::Pressed => 0x38383b,
                Phase::Hover | Phase::Selected => 0x303033,
                _ => 0x18181a,
            }))
            .border_0()
            .shadow(vec![])
            .text_color(rgb(if phase == Phase::Disabled {
                0x777777
            } else {
                0xf5f5f5
            }));
    }
    element
        .bg(match phase {
            Phase::Pressed => Color::PressedSurface.get(),
            Phase::Hover | Phase::Selected => Color::HoverSurface.get(),
            _ => Color::RowSurface.get(),
        })
        .border_0()
        .shadow(vec![])
        .text_color(if phase == Phase::Disabled {
            Color::DisabledText.get()
        } else if role == Role::Danger {
            Color::Error.get()
        } else {
            Color::PrimaryText.get()
        })
}

/// Recorder controls share the translucent dock material, including Record.
/// Ink carries action/disabled semantics; no opaque Primary/Record fill is added.
fn frosted_bar<T: Styled>(element: T, role: Role, phase: Phase, hover: f32, press: f32) -> T {
    let base = if matches!(role, Role::Primary | Role::Record | Role::Selection) {
        Color::HoverSurface.get()
    } else {
        Color::RowSurface.get()
    };
    let face = mix(
        base,
        Color::PrimaryText.get(),
        (0.035 * hover + 0.06 * press + if phase == Phase::Selected { 0.05 } else { 0. })
            .clamp(0., 0.10),
    );
    let ink = if phase == Phase::Disabled {
        Color::DisabledText
    } else if role == Role::Record {
        Color::Record
    } else if role == Role::Danger {
        Color::Error
    } else {
        Color::PrimaryText
    };
    element
        .bg(face)
        .border_0()
        .shadow(vec![])
        .text_color(ink.get())
}

pub(crate) fn window_control<T: Styled>(element: T, phase: Phase, danger: bool) -> T {
    let ink = if phase == Phase::Disabled {
        Color::DisabledText
    } else if danger && matches!(phase, Phase::Hover | Phase::Pressed) {
        Color::Error
    } else if matches!(phase, Phase::Hover | Phase::Pressed) {
        Color::PrimaryText
    } else {
        Color::SecondaryText
    };
    element
        .border_0()
        .bg(gpui::rgba(0))
        .shadow(vec![])
        .text_color(ink.get())
}

/// The segmented track owns the only moving face. Buttons own input and ink.
pub(crate) fn segment<T: Styled>(element: T, phase: Phase) -> T {
    element
        .border_0()
        .bg(gpui::rgba(0))
        .shadow(vec![])
        .text_color(
            if phase == Phase::Disabled {
                Color::DisabledText
            } else {
                Color::PrimaryText
            }
            .get(),
        )
}

/// Navigation uses the workspace source-row material, independent of Filled.
pub(crate) fn navigation<T: Styled>(element: T, phase: Phase, hover: f32, press: f32) -> T {
    let selected = phase == Phase::Selected;
    let hover = if phase == Phase::Disabled {
        0.
    } else {
        hover.clamp(0., 1.)
    };
    element
        .bg(if selected {
            Color::HoverSurface.base()
        } else {
            Color::RowSurface
                .base()
                .opacity((hover * 0.85 + press * 0.15).clamp(0., 1.))
        })
        .border_0()
        .shadow(vec![])
        .text_color(if phase == Phase::Disabled {
            Color::DisabledText.base()
        } else if selected {
            Color::PrimaryText.base()
        } else {
            mix(
                Color::SecondaryText.base(),
                Color::PrimaryText.base(),
                hover,
            )
        })
}

/// Settings panels share the Recorder dock tint and quiet edge.
pub(crate) fn panel<T: Styled>(element: T) -> T {
    element
        .bg(Color::DockSurface.get())
        .border_color(Color::DefaultBorder.get().opacity(0.65))
        .shadow(vec![])
}

fn switch_colors(checked: bool, thumb: bool) -> (Hsla, Hsla) {
    if checked {
        return if thumb {
            (rgb(0xffffff).into(), rgb(0xe9f5ee).into())
        } else {
            (rgb(0x72cfa0).into(), rgb(0x4baa7c).into())
        };
    }
    if ThemeId::current() == ThemeId::Black {
        let (a, b) = match (checked, thumb) {
            (true, true) => (0x3c424e, 0x242933),
            (false, true) => (0xb2b6c0, 0x858b98),
            (true, false) => (0xd6dce7, 0xa5afc3),
            (false, false) => (0x16171a, 0x24262b),
        };
        return (rgb(a).into(), rgb(b).into());
    }
    let base = if thumb {
        if checked {
            Color::OnAccent.get()
        } else {
            Color::SecondaryText.get()
        }
    } else if checked {
        Color::Accent.get()
    } else {
        Color::DefaultBorder.get()
    };
    (mix(base, rgb(0xffffff).into(), 0.08), base)
}
pub(crate) fn switch_face_at(position: f32, thumb: bool) -> Background {
    let off = switch_colors(false, thumb);
    let on = switch_colors(true, thumb);
    let t = position.clamp(0., 1.);
    gradient(mix(off.0, on.0, t), mix(off.1, on.1, t))
}
pub(crate) fn switch_shadows_at(position: f32, thumb: bool) -> Vec<BoxShadow> {
    if super::low_cost_effects() {
        return vec![];
    }
    let t = position.clamp(0., 1.);
    if thumb {
        vec![
            inset(1., 0., white(0.38 - 0.20 * t)),
            cast(1., 2., 0., black(0.38)),
        ]
    } else {
        vec![
            BoxShadow::new(px(0.), px(0.), white(0.08 + 0.10 * t))
                .spread_radius(px(1.))
                .inset(),
            inset(1., 1., black(0.25 - 0.12 * t)),
        ]
    }
}

/// Home and Settings share a raised material. Hover and press values arrive
/// continuously from the button spring, so fill and elevation move together.
pub(crate) fn raised<T: Styled>(element: T, role: Role, phase: Phase, hover: f32, press: f32) -> T {
    surface(element, role, phase, hover, press, false, true)
}

/// Quiet Command Deck utilities. Static state styles add no animation work.
pub(crate) fn deck_surface<T: Styled>(element: T, phase: Phase) -> T {
    let disabled = phase == Phase::Disabled;
    let hovered = matches!(phase, Phase::Hover | Phase::Pressed);
    let face = if hovered {
        Color::HoverSurface.get()
    } else {
        Color::RowSurface.get()
    };
    let face = if phase == Phase::Pressed {
        mix(face, Color::PrimaryText.get(), 0.025)
    } else {
        face
    };
    let shadows = if disabled || super::low_cost_effects() {
        vec![]
    } else {
        vec![
            inset(1., 0., white(if hovered { 0.055 } else { 0.025 })),
            cast(1., if hovered { 5. } else { 3. }, -1., black(0.16)),
        ]
    };
    element
        .bg(face)
        .border_1()
        .border_color(
            Color::SubtleBorder
                .get()
                .opacity(if hovered { 0.65 } else { 0.25 }),
        )
        .shadow(shadows)
        .text_color(
            if disabled {
                Color::DisabledText
            } else {
                Color::PrimaryText
            }
            .get(),
        )
}

/// Opt-in candidate material. Only the native catalogue supplies these values.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::{self, SurfacePreference};

    #[test]
    fn unfilled_buttons_are_borderless_and_switch_material_is_independent() {
        for theme_id in ThemeId::ALL {
            theme_id.activate();
            for surface in SurfacePreference::ALL {
                surface.activate();
                theme::set_accent(Some(0xb06b25));
                let accent = Color::Accent.get();
                theme::set_filled_buttons(true);
                let panel = Color::PrimarySurface.get();
                let on = switch_colors(true, false);
                let off = switch_colors(false, true);
                theme::set_filled_buttons(false);
                assert_eq!(switch_colors(true, false), on);
                assert_eq!(switch_colors(false, true), off);
                assert_eq!(Color::Accent.get(), accent);
                assert_eq!(Color::PrimarySurface.get(), panel);
                assert_eq!(ThemeId::current(), theme_id);
                for role in [Role::Primary, Role::Secondary, Role::Record, Role::Danger] {
                    let mut button = apply(gpui::div(), role, Phase::Rest, false);
                    assert_eq!(button.style().border_widths.top, Some(px(0.).into()));
                    assert!(
                        button
                            .style()
                            .background
                            .as_ref()
                            .unwrap()
                            .color()
                            .unwrap()
                            .is_transparent()
                    );
                    let mut hover = raised(gpui::div(), role, Phase::Hover, 1., 0.);
                    let fill = hover
                        .style()
                        .background
                        .as_ref()
                        .unwrap()
                        .color()
                        .unwrap()
                        .as_solid()
                        .unwrap();
                    assert!(fill.a <= 0.081);
                    assert_ne!(fill, accent);
                    for phase in [
                        Phase::Rest,
                        Phase::Hover,
                        Phase::Pressed,
                        Phase::Selected,
                        Phase::Disabled,
                    ] {
                        let mut button = apply(gpui::div().border_1(), role, phase, false);
                        assert_eq!(button.style().border_widths.top, Some(px(0.).into()));
                        if phase == Phase::Selected {
                            assert!(
                                !button
                                    .style()
                                    .background
                                    .as_ref()
                                    .unwrap()
                                    .color()
                                    .unwrap()
                                    .is_transparent(),
                                "selection must remain visible without an outline"
                            );
                        }
                    }
                }
            }
        }
        theme::set_accent(None);
        theme::set_filled_buttons(true);
        ThemeId::Black.activate();
        SurfacePreference::Frosted.activate();
    }
    #[test]
    fn amoled_caption_controls_are_unboxed_and_record_bars_remain_translucent() {
        use gpui::Styled;
        for theme_id in ThemeId::ALL {
            theme_id.activate();
            SurfacePreference::Frosted.activate();
            theme::set_low_cost_effects(false);
            super::super::surface_treatment::set_frosted_transparency(50);
            for filled in [false, true] {
                theme::set_filled_buttons(filled);
                for phase in [Phase::Rest, Phase::Hover, Phase::Pressed, Phase::Disabled] {
                    let mut caption = window_control(gpui::div().border_1(), phase, true);
                    let style = caption.style();
                    assert_eq!(style.border_widths.top, Some(px(0.).into()));
                    assert!(
                        style
                            .background
                            .as_ref()
                            .unwrap()
                            .color()
                            .unwrap()
                            .is_transparent()
                    );
                    assert!(style.box_shadow.as_ref().unwrap().is_empty());
                    for role in [Role::Primary, Role::Secondary, Role::Record] {
                        let mut bar = frosted_bar(gpui::div(), role, phase, 0., 0.);
                        assert_eq!(bar.style().border_widths.top, Some(px(0.).into()));
                        assert!(bar.style().box_shadow.as_ref().unwrap().is_empty());
                        let color = bar
                            .style()
                            .background
                            .as_ref()
                            .unwrap()
                            .color()
                            .unwrap()
                            .as_solid()
                            .unwrap();
                        assert_eq!(
                            color,
                            if role == Role::Secondary {
                                Color::RowSurface.get()
                            } else {
                                Color::HoverSurface.get()
                            }
                        );
                    }
                }
            }
        }
        theme::set_filled_buttons(false);
        super::super::surface_treatment::set_frosted_transparency(20);
        ThemeId::Black.activate();
    }

    #[test]
    fn settings_navigation_uses_flat_display_selection_without_glow() {
        for theme_id in ThemeId::ALL {
            theme_id.activate();
            theme::set_low_cost_effects(false);
            let mut idle = navigation(gpui::div(), Phase::Rest, 0., 0.);
            assert!(idle.style().box_shadow.as_ref().unwrap().is_empty());
            let mut halfway = navigation(gpui::div(), Phase::Rest, 0.5, 0.);
            let mut hovered = navigation(gpui::div(), Phase::Rest, 1., 0.);
            let half = halfway.style().box_shadow.as_ref().unwrap();
            let full = hovered.style().box_shadow.as_ref().unwrap();
            assert!(half.is_empty() && full.is_empty());
            let mut selected = navigation(gpui::div(), Phase::Selected, 0., 0.);
            assert_eq!(
                selected
                    .style()
                    .background
                    .as_ref()
                    .unwrap()
                    .color()
                    .unwrap()
                    .as_solid()
                    .unwrap(),
                Color::HoverSurface.get()
            );
            assert_eq!(hovered.style().border_widths.top, Some(px(0.).into()));
            theme::set_low_cost_effects(true);
            let mut cheap = navigation(gpui::div(), Phase::Rest, 1., 0.);
            assert!(cheap.style().box_shadow.as_ref().unwrap().is_empty());
            theme::set_low_cost_effects(false);
        }
        ThemeId::Black.activate();
    }
    #[test]
    fn navigation_feedback_and_ink_ignore_frosted_transparency() {
        use super::super::surface_treatment::{self, SurfacePreference};
        use gpui::Styled;
        for theme in ThemeId::ALL {
            theme.activate();
            SurfacePreference::Opaque.activate();
            let mut original = navigation(gpui::div(), Phase::Rest, 0.6, 0.);
            let original = original.style().clone();
            SurfacePreference::Frosted.activate();
            for percent in [0, 20, 50, 100] {
                surface_treatment::set_frosted_transparency(percent);
                let mut face = navigation(gpui::div(), Phase::Rest, 0.6, 0.);
                assert_eq!(face.style().background, original.background);
                assert_eq!(face.style().text.color, original.text.color);
            }
        }
        ThemeId::Black.activate();
        surface_treatment::set_frosted_transparency(20);
    }
}
