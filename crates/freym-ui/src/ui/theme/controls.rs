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
fn shadow(y: f32, blur: f32, color: u32, inset: bool) -> BoxShadow {
    let shadow = BoxShadow::new(px(0.), px(y), rgba(color).into()).blur_radius(px(blur));
    if inset { shadow.inset() } else { shadow }
}
fn shadows(primary: bool, phase: Phase) -> Vec<BoxShadow> {
    if super::low_cost_effects() {
        return vec![];
    }
    if phase == Phase::Disabled {
        return vec![
            shadow(1., 0., 0xffffff07, true),
            shadow(1., 2., 0x00000030, false),
        ];
    }
    if ThemeId::current() == ThemeId::ClassicLight {
        return match phase {
            Phase::Pressed => vec![shadow(1., 3., 0x00000018, true)],
            _ => vec![
                shadow(1., 0., 0xffffffc0, true),
                shadow(-1., 0., 0x00000012, true),
                shadow(2., 4., 0x0000000e, false),
            ],
        };
    }
    match (primary, phase) {
        (true, Phase::Pressed) => vec![
            shadow(2., 4., 0x00000024, true),
            shadow(1., 0., 0xffffff40, true),
            shadow(1., 2., 0x00000060, false),
        ],
        (true, Phase::Hover) => vec![
            shadow(1., 0., 0xffffffff, true),
            shadow(-1., 0., 0xb6b5b266, true),
            shadow(3., 6., 0x00000070, false),
            shadow(0., 14., 0xffffff12, false),
        ],
        (true, _) => vec![
            shadow(1., 0., 0xffffffef, true),
            shadow(-1., 0., 0xb6b5b273, true),
            shadow(2., 4., 0x00000070, false),
            shadow(0., 12., 0xffffff0d, false),
        ],
        (_, Phase::Pressed) => vec![
            shadow(2., 3., 0x00000055, true),
            shadow(1., 0., 0xffffff04, true),
            shadow(1., 2., 0x00000050, false),
        ],
        (_, Phase::Hover) => vec![
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
pub(crate) fn apply<T: Styled>(mut element: T, role: Role, phase: Phase, flat: bool) -> T {
    let mut style = StyleRefinement::default();
    let base = match phase {
        Phase::Hover | Phase::Selected => Color::HoverSurface.get(),
        Phase::Pressed => Color::PressedSurface.get(),
        _ => Color::ElevatedSurface.get(),
    };
    let quiet = role == Role::Quiet;
    let filled = super::filled_buttons() && !flat;
    let primary = role == Role::Primary;
    let mut ink = if phase == Phase::Disabled {
        Color::MutedText.get()
    } else {
        Color::PrimaryText.get()
    };
    if flat || !filled {
        style = style.bg(if quiet && phase == Phase::Rest {
            base.opacity(0.)
        } else if flat && phase == Phase::Rest {
            Color::PrimarySurface.get()
        } else {
            base
        });
        if primary && !flat {
            style = style.bg(Color::Accent.get());
            ink = Color::OnAccent.get();
        }
        style = style.shadow(vec![]);
    } else if quiet {
        style = style
            .bg(match phase {
                Phase::Hover => gradient(
                    Color::PrimaryText.get().opacity(0.04),
                    Color::PrimaryText.get().opacity(0.02),
                ),
                Phase::Pressed => rgba(0xffffff08).into(),
                _ => rgba(0x00000000).into(),
            })
            .shadow(vec![]);
    } else {
        let amoled = ThemeId::current() == ThemeId::AmoledBlack;
        let (top, bottom): (Hsla, Hsla) = if phase == Phase::Disabled && amoled {
            (rgb(0x1b1b1e).into(), rgb(0x171719).into())
        } else if role == Role::Record {
            let (a, b) = match phase {
                Phase::Hover => (0xea4950, 0xd5363e),
                Phase::Pressed => (0xb52630, 0xc93039),
                _ => (0xde3f45, 0xc72d35),
            };
            ink = rgb(0xffffff).into();
            (rgb(a).into(), rgb(b).into())
        } else if primary {
            if amoled {
                ink = rgb(0x202124).into();
                let (a, b) = match phase {
                    Phase::Hover => (0xfffefb, 0xeeedea),
                    Phase::Pressed => (0xd7d6d3, 0xe4e3df),
                    _ => (0xf5f4f1, 0xe3e2df),
                };
                (rgb(a).into(), rgb(b).into())
            } else {
                let base = if ThemeId::current().is_motion() {
                    ink = Color::OnAccent.get();
                    Color::Accent.get()
                } else if ThemeId::current() == ThemeId::FreymExperimental {
                    Color::HoverSurface.get()
                } else {
                    ink = rgb(0x202124).into();
                    rgb(0xeeeff2).into()
                };
                (
                    mix(base, rgb(0xffffff).into(), 0.08),
                    mix(base, rgb(0x000000).into(), 0.06),
                )
            }
        } else if amoled {
            let (a, b) = match (role, phase) {
                (_, Phase::Pressed) => (0x1b1b1e, 0x232326),
                (Role::Selection, Phase::Selected) => (0x303034, 0x252529),
                (Role::Selection, Phase::Hover) => (0x2c2c30, 0x222225),
                (Role::Selection, _) => (0x252528, 0x1b1b1e),
                (_, Phase::Hover) => (0x323235, 0x27272a),
                _ => (0x2a2a2d, 0x202023),
            };
            (rgb(a).into(), rgb(b).into())
        } else {
            (
                mix(
                    base,
                    if phase == Phase::Pressed {
                        rgb(0x000000).into()
                    } else {
                        rgb(0xffffff).into()
                    },
                    0.1,
                ),
                base,
            )
        };
        style = style
            .bg(gradient(top, bottom))
            .shadow(shadows(primary, phase));
    }
    if role == Role::Danger {
        ink = Color::Error.get();
    }
    element.style().refine(&style.text_color(ink));
    element
}

pub(crate) fn switch_face(checked: bool, thumb: bool) -> Background {
    if ThemeId::current() == ThemeId::AmoledBlack && super::filled_buttons() {
        let (a, b) = match (checked, thumb) {
            (true, true) => (0x42434a, 0x27282f),
            (false, true) => (0xa8aab0, 0x85878e),
            (true, false) => (0xdedee2, 0xbcbec5),
            (false, false) => (0x202023, 0x29292d),
        };
        return gradient(rgb(a).into(), rgb(b).into());
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
    gradient(mix(base, rgb(0xffffff).into(), 0.08), base)
}
