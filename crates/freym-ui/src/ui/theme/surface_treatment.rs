//! Palette-independent window frost. Native compositor blur, no render loop.
use super::{ThemeId, colors::Color};
use gpui::{App, Hsla, Window, WindowBackgroundAppearance, rgb};
use serde::{Deserialize, Serialize};
use std::cell::Cell;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfacePreference {
    Opaque,
    #[default]
    #[serde(alias = "theme_default", other)]
    Frosted,
}

thread_local! {
    static ACTIVE: Cell<SurfacePreference> = const { Cell::new(SurfacePreference::Frosted) };
    static TINTS: Cell<Option<(ThemeId, [Hsla; 6])>> = const { Cell::new(None) };
    static TRANSPARENCY: Cell<u8> = const { Cell::new(20) };
}

pub(crate) fn set_frosted_transparency(percent: u8) {
    let percent = percent.min(100);
    if TRANSPARENCY.replace(percent) != percent {
        TINTS.set(None);
    }
}

impl SurfacePreference {
    pub const ALL: [Self; 2] = [Self::Frosted, Self::Opaque];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Frosted => "Frosted",
            Self::Opaque => "Opaque",
        }
    }

    pub fn current() -> Self {
        ACTIVE.get()
    }

    pub(crate) fn activate(self) {
        ACTIVE.set(self);
    }
}

pub fn is_frosted() -> bool {
    !super::low_cost_effects()
        && cfg!(any(target_os = "windows", target_os = "macos"))
        && composition_supports_alpha()
        && match SurfacePreference::current() {
            SurfacePreference::Frosted => true,
            SurfacePreference::Opaque => false,
        }
}

fn composition_supports_alpha() -> bool {
    #[cfg(target_os = "windows")]
    {
        // Match the pinned GPUI backend's startup switch. Its HWND fallback
        // uses DXGI_ALPHA_MODE_IGNORE and cannot display genuine translucency.
        static SUPPORTED: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
            !matches!(
                std::env::var("GPUI_DISABLE_DIRECT_COMPOSITION").as_deref(),
                Ok("1" | "true")
            )
        });
        *SUPPORTED
    }
    #[cfg(not(target_os = "windows"))]
    {
        true
    }
}

/// Transparent compositing remains necessary for the recorder's child HWND.
/// Its preview hole and pixels are independent from the chrome's tint.
pub(crate) fn window_background(transparent: bool) -> WindowBackgroundAppearance {
    if is_frosted() {
        WindowBackgroundAppearance::Blurred
    } else if transparent {
        WindowBackgroundAppearance::Transparent
    } else {
        WindowBackgroundAppearance::Opaque
    }
}

pub(crate) fn apply_window(window: &mut Window, cx: &mut App, transparent: bool) {
    let appearance = window_background(transparent);
    let applied = window.use_keyed_state("freym-window-surface", cx, |_, _| None);
    if *applied.read(cx) != Some(appearance) {
        window.set_background_appearance(appearance);
        applied.update(cx, |last, _| *last = Some(appearance));
    }
}

/// A titlebar is painted over the shell; don't double the shell's tint there.
pub(crate) fn titlebar_background() -> Hsla {
    if is_frosted() {
        Color::Background.base().opacity(0.)
    } else {
        Color::Background.get()
    }
}

/// Dock headers keep their existing coverage, with the same black as the panel.
pub(crate) fn dock_header_color() -> Hsla {
    if ThemeId::current() == ThemeId::Black {
        Color::DockSurface
            .base()
            .alpha(Color::ElevatedSurface.get().a)
    } else {
        Color::ElevatedSurface.get()
    }
}

// Preserve every legacy 0–50 alpha. The newly opened range then fades its
// existing endpoint to zero, without rescaling any saved preference.
fn extended_alpha(minimum: f32, percent: u8) -> f32 {
    if percent <= 50 {
        let openness = percent as f32 / 50.;
        1. - (1. - minimum) * openness
    } else {
        minimum * (100 - percent.min(100)) as f32 / 50.
    }
}

pub(crate) fn color(role: Color, base: Hsla) -> Hsla {
    let index = match role {
        Color::Background => 0,
        Color::PrimarySurface => 1,
        Color::ElevatedSurface => 2,
        Color::PreviewSurface => 3,
        Color::RowSurface => 4,
        Color::DockSurface => 5,
        _ => return base, // Text, accents and interaction states stay authored.
    };
    if !is_frosted() {
        return base.alpha(1.);
    }
    let theme = ThemeId::current();
    if let Some((cached_theme, tints)) = TINTS.get()
        && theme == cached_theme
    {
        return tints[index];
    }
    // Opaque returns above, before the Frosted cache. Restore the original
    // Frosted panel/elevated coverage (40%/50% at the default setting) without
    // applying the shell's desktop-readability floor to every child surface.
    let percent = TRANSPARENCY.get();
    let coverage = 1. - TRANSPARENCY.get() as f32 / 100.;
    let shell = Color::Background.base();
    let minimum = readable_shell_alpha(shell, 4.5);
    let dock = Color::DockSurface.base();
    // Use the legacy panel palette for coverage; changing only RGB must not
    // silently alter the existing dock transparency.
    let dock_minimum = readable_shell_alpha(Color::PrimarySurface.base(), 3.);
    let tints = [
        shell.alpha(extended_alpha(minimum, percent)),
        Color::PrimarySurface.base().alpha(0.50 * coverage),
        Color::ElevatedSurface.base().alpha(0.625 * coverage),
        Color::PreviewSurface.base().alpha(0.80 * coverage),
        Color::RowSurface.base().alpha(0.625 * coverage),
        // Dock panels replace the shell beneath them, rather than layering
        // another translucent fill over it. Keep labels readable on desktop.
        dock.alpha(extended_alpha(dock_minimum, percent)),
    ];
    TINTS.set(Some((theme, tints)));
    tints[index]
}

/// GPUI overlays cannot blur siblings behind them. An opaque popup prevents
/// underlying controls and text from showing through its content.
pub(crate) fn overlay_color() -> Hsla {
    Color::ElevatedSurface.base().alpha(1.)
}

fn readable_shell_alpha(base: Hsla, secondary_contrast: f32) -> f32 {
    // Only the outer shell has an unknown desktop behind it. Compute once per
    // cached palette/opacity change; interpolate the entire slider range so
    // high transparency settings do not collapse onto one clamped value.
    let desktops = [rgb(0).into(), rgb(0xffffff).into()];
    for alpha in 50..=100 {
        let alpha = alpha as f32 / 100.;
        if desktops.iter().all(|desktop| {
            let painted = flatten(base.alpha(alpha), *desktop);
            [Color::PrimaryText, Color::SecondaryText, Color::MutedText]
                .into_iter()
                .all(|text| {
                    contrast(text.base(), painted)
                        >= if text == Color::PrimaryText {
                            4.5
                        } else {
                            secondary_contrast
                        }
                })
        }) {
            return alpha;
        }
    }
    1.
}

fn flatten(front: Hsla, back: Hsla) -> Hsla {
    let front: gpui::Rgba = front.into();
    let back: gpui::Rgba = back.into();
    gpui::Rgba {
        r: front.r * front.a + back.r * (1. - front.a),
        g: front.g * front.a + back.g * (1. - front.a),
        b: front.b * front.a + back.b * (1. - front.a),
        a: 1.,
    }
    .into()
}

fn contrast(a: Hsla, b: Hsla) -> f32 {
    fn luminance(color: Hsla) -> f32 {
        let color: gpui::Rgba = color.into();
        let linear = |v: f32| {
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
    }
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            ThemeId::Black.activate();
            SurfacePreference::Frosted.activate();
            set_frosted_transparency(20);
            super::super::set_low_cost_effects(false);
        }
    }

    #[test]
    fn opaque_black_preserves_the_original_amoled_palette() {
        let _reset = Reset;
        ThemeId::Black.activate();
        SurfacePreference::Opaque.activate();
        for (role, rgb_value) in [
            (Color::Background, 0x000000),
            (Color::PrimarySurface, 0x101010),
            (Color::DockSurface, 0x000000),
            (Color::ElevatedSurface, 0x191919),
            (Color::PreviewSurface, 0x121214),
            (Color::RowSurface, 0x191919),
            (Color::HoverSurface, 0x242424),
            (Color::PressedSurface, 0x262626),
        ] {
            assert_eq!(role.get(), rgb(rgb_value).into(), "{role:?}");
        }
    }

    #[test]
    fn frosted_restores_translucent_layers_independently_of_the_opaque_palette() {
        let _reset = Reset;
        SurfacePreference::Frosted.activate();
        super::super::set_low_cost_effects(false);
        if !is_frosted() {
            return;
        }
        for theme in ThemeId::ALL {
            theme.activate();
            for (percent, panel, elevated, preview) in
                [(20, 0.40, 0.50, 0.64), (50, 0.25, 0.3125, 0.40)]
            {
                set_frosted_transparency(percent);
                for (role, alpha) in [
                    (Color::PrimarySurface, panel),
                    (Color::ElevatedSurface, elevated),
                    (Color::PreviewSurface, preview),
                    (Color::RowSurface, elevated),
                ] {
                    assert!((role.get().a - alpha).abs() < 0.00001, "{theme:?} {role:?}");
                    assert_eq!(role.get().alpha(1.), role.base());
                }
                assert!(
                    Color::Background.get().a <= 1. - 0.10 * percent as f32 / 50.,
                    "{theme:?}: the shell must retain measurable desktop transmission"
                );
                // The root must actually transmit the desktop, and adding a
                // panel must not replace it with an opaque parent surface.
                let composite = |desktop| {
                    flatten(
                        Color::PrimarySurface.get(),
                        flatten(Color::Background.get(), rgb(desktop).into()),
                    )
                };
                assert_ne!(composite(0), composite(0xffffff));
            }
        }
    }

    #[test]
    fn switching_appearance_and_opacity_never_leaks_frosted_values_into_opaque() {
        let _reset = Reset;
        super::super::set_low_cost_effects(false);
        let surfaces = [
            Color::Background,
            Color::PrimarySurface,
            Color::DockSurface,
            Color::ElevatedSurface,
            Color::PreviewSurface,
            Color::RowSurface,
        ];
        for theme in ThemeId::ALL {
            theme.activate();
            SurfacePreference::Frosted.activate();
            set_frosted_transparency(50);
            let frosted = surfaces.map(Color::get);
            for percent in [0, 20, 50, 255, 50] {
                SurfacePreference::Opaque.activate();
                set_frosted_transparency(percent);
                assert_eq!(surfaces.map(Color::get), surfaces.map(Color::base));
                assert_eq!(overlay_color(), Color::ElevatedSurface.base());
                assert_eq!(titlebar_background(), Color::Background.base());
            }
            SurfacePreference::Frosted.activate();
            assert_eq!(surfaces.map(Color::get), frosted, "restores cached tint");
            if is_frosted() {
                assert!(frosted.iter().all(|color| color.a < 1.));
                set_frosted_transparency(0);
                assert_ne!(surfaces.map(Color::get), frosted, "invalidates tint cache");
            }
        }
    }

    #[test]
    fn frost_preserves_palette_and_text_contrast_over_unknown_desktops() {
        let _reset = Reset;
        SurfacePreference::Frosted.activate();
        super::super::set_low_cost_effects(false);
        for theme in ThemeId::ALL {
            theme.activate();
            assert_eq!(Color::Accent.get(), Color::Accent.base());
            assert_eq!(
                Color::PreviewSurface.get().alpha(1.),
                Color::PreviewSurface.base()
            );
            if !is_frosted() {
                continue;
            }
            assert_eq!(window_background(true), WindowBackgroundAppearance::Blurred);
            assert_eq!(
                window_background(false),
                WindowBackgroundAppearance::Blurred
            );
            assert_eq!(titlebar_background().a, 0.);
            assert!(Color::Background.get().a < 1., "{theme:?}");
            for desktop in [0, 0xffffff, 0xff0000, 0x00ff00, 0x0000ff] {
                let shell = flatten(Color::Background.get(), rgb(desktop).into());
                for painted in [
                    shell,
                    flatten(Color::PrimarySurface.get(), shell),
                    flatten(Color::ElevatedSurface.get(), shell),
                    flatten(Color::PreviewSurface.get(), shell),
                    flatten(Color::RowSurface.get(), shell),
                ] {
                    assert!(
                        contrast(Color::PrimaryText.get(), painted) >= 4.5,
                        "{theme:?}"
                    );
                    for text in [Color::SecondaryText, Color::MutedText] {
                        assert!(contrast(text.get(), painted) >= 3., "{theme:?} {text:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn single_dock_material_is_more_translucent_than_the_previous_stacked_fill() {
        let _reset = Reset;
        SurfacePreference::Frosted.activate();
        super::super::set_low_cost_effects(false);
        if !is_frosted() {
            return;
        }
        for theme in ThemeId::ALL {
            theme.activate();
            for percent in [10, 20, 30, 40, 50] {
                set_frosted_transparency(percent);
                let dock = Color::DockSurface.get();
                let previous = 1.
                    - (1. - Color::Background.get().a)
                        * (1. - Color::PrimarySurface.get().a * 0.80);
                assert!(dock.a < previous - 0.02, "{theme:?} {percent}");
                assert_eq!(dock.alpha(1.), Color::DockSurface.base());
                for desktop in [rgb(0).into(), rgb(0xffffff).into()] {
                    let painted = flatten(dock, desktop);
                    assert!(contrast(Color::PrimaryText.get(), painted) >= 4.5);
                    assert!(contrast(Color::SecondaryText.get(), painted) >= 3.);
                    assert!(contrast(Color::MutedText.get(), painted) >= 3.);
                }
            }
        }
    }

    #[test]
    fn transparency_invalidates_tint_cache_without_fading_text_or_changing_surface_hues() {
        let _reset = Reset;
        ThemeId::Black.activate();
        SurfacePreference::Frosted.activate();
        super::super::set_low_cost_effects(false);
        if !is_frosted() {
            return;
        }
        let text = Color::PrimaryText.get();
        let preview_color = Color::PreviewSurface.base();
        let mut previous = 1.;
        for percent in [0, 10, 20, 30, 40, 50] {
            set_frosted_transparency(percent);
            let shell = Color::Background.get();
            assert!(
                percent == 0 || shell.a < previous - 0.0001,
                "each setting must change alpha"
            );
            assert_eq!(shell, Color::Background.get(), "cached value");
            previous = shell.a;
            assert_eq!(Color::PrimaryText.get(), text);
            assert_eq!(Color::PreviewSurface.get().alpha(1.), preview_color);
            for desktop in [rgb(0).into(), rgb(0xffffff).into()] {
                assert!(contrast(text, flatten(shell, desktop)) >= 4.5);
            }
        }
        assert!(previous < 1.);
        set_frosted_transparency(255);
        assert_eq!(Color::Background.get().a, 0.);
    }

    #[test]
    fn amoled_dock_preserves_legacy_alpha_and_full_transparency_keeps_text() {
        let _reset = Reset;
        super::super::set_low_cost_effects(false);
        SurfacePreference::Frosted.activate();
        if !is_frosted() {
            return;
        }
        for theme in ThemeId::ALL {
            theme.activate();
            let minimum = readable_shell_alpha(Color::PrimarySurface.base(), 3.);
            let ink = Color::PrimaryText.get();
            for percent in [0, 10, 20, 30, 40, 50] {
                set_frosted_transparency(percent);
                let legacy = 1. - (1. - minimum) * (percent as f32 / 50.);
                assert_eq!(Color::DockSurface.get().a, legacy);
                assert_eq!(dock_header_color().a, Color::ElevatedSurface.get().a);
            }
            for percent in [75, 100, 255] {
                set_frosted_transparency(percent);
                assert_eq!(
                    Color::DockSurface.get().a,
                    minimum * (100 - percent.min(100)) as f32 / 50.
                );
                assert_eq!(Color::PrimaryText.get(), ink);
                if percent >= 100 {
                    for role in [
                        Color::Background,
                        Color::PrimarySurface,
                        Color::DockSurface,
                        Color::ElevatedSurface,
                        Color::PreviewSurface,
                        Color::RowSurface,
                    ] {
                        assert_eq!(role.get().a, 0., "{theme:?} {role:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn both_palettes_follow_the_entire_opacity_range_including_secondary_surfaces() {
        let _reset = Reset;
        SurfacePreference::Frosted.activate();
        super::super::set_low_cost_effects(false);
        if !is_frosted() {
            return;
        }
        for theme in ThemeId::ALL {
            theme.activate();
            let mut previous = 1.1;
            for percent in [0, 10, 20, 30, 40, 50] {
                set_frosted_transparency(percent);
                let shell = Color::Background.get();
                assert!(shell.a < previous);
                previous = shell.a;
                assert_eq!(overlay_color().a, 1.);
                for desktop in [rgb(0).into(), rgb(0xffffff).into()] {
                    let shell = flatten(shell, desktop);
                    for painted in [
                        shell,
                        flatten(Color::PrimarySurface.get(), shell),
                        flatten(Color::ElevatedSurface.get(), shell),
                        flatten(Color::PreviewSurface.get(), shell),
                        flatten(Color::RowSurface.get(), shell),
                        flatten(overlay_color(), shell),
                    ] {
                        assert!(contrast(Color::PrimaryText.get(), painted) >= 4.5);
                        assert!(contrast(Color::SecondaryText.get(), painted) >= 3.);
                        assert!(contrast(Color::MutedText.get(), painted) >= 3.);
                    }
                }
            }
            SurfacePreference::Opaque.activate();
            assert_eq!(overlay_color().a, 1.);
            SurfacePreference::Frosted.activate();
        }
    }

    #[test]
    fn opaque_and_low_cost_restore_solid_surfaces_without_losing_frost_choice() {
        let _reset = Reset;
        for theme in ThemeId::ALL {
            theme.activate();
            for preference in SurfacePreference::ALL {
                preference.activate();
                super::super::set_low_cost_effects(true);
                assert!(!is_frosted());
                assert_eq!(
                    window_background(true),
                    WindowBackgroundAppearance::Transparent
                );
                assert_eq!(window_background(false), WindowBackgroundAppearance::Opaque);
                assert_eq!(SurfacePreference::current(), preference);
                for role in [
                    Color::Background,
                    Color::PrimarySurface,
                    Color::DockSurface,
                    Color::ElevatedSurface,
                    Color::PreviewSurface,
                    Color::RowSurface,
                ] {
                    assert_eq!(role.get().a, 1.);
                }
                super::super::set_low_cost_effects(false);
            }
            SurfacePreference::Opaque.activate();
            assert!(!is_frosted());
            assert_eq!(Color::PrimarySurface.get().a, 1.);
        }
    }
}
