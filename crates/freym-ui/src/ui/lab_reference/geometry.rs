//! Port of apps/ui-lab/src/anchored-geometry.ts, in logical pixels.
//! Rectangle calculation only; this does not claim a native rounded clip renderer.
#![allow(dead_code)]

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Origin {
    Start,
    Anchor,
    End,
    Auto,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Placement {
    Auto,
    Below,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    pub panel: Rect,
    pub seed: Rect,
    pub upward: bool,
}
pub const OPEN_MS: u64 = 200;
pub const CLOSE_MS: u64 = 145;
fn clamp(v: f32, min: f32, max: f32) -> f32 {
    v.max(min).min(max.max(min))
}

pub fn anchored_geometry(
    anchor: Rect,
    desired: (f32, f32),
    viewport: Rect,
    origin: Origin,
    gap: f32,
    row: Option<Rect>,
    placement: Placement,
) -> Option<Geometry> {
    // UI Lab assumes a usable viewport. Reject invalid/native-minimized bounds.
    if [
        anchor.left,
        anchor.top,
        anchor.width,
        anchor.height,
        desired.0,
        desired.1,
        viewport.left,
        viewport.top,
        viewport.width,
        viewport.height,
        gap,
    ]
    .iter()
    .any(|n| !n.is_finite())
        || viewport.width <= 32.
        || viewport.height <= 32.
        || desired.0 <= 0.
        || desired.1 <= 0.
        || anchor.width <= 0.
        || anchor.height <= 0.
    {
        return None;
    }
    let right = viewport.left + viewport.width - 16.;
    let bottom = viewport.top + viewport.height - 16.;
    let width = desired.0.min(viewport.width - 32.);
    let mut left = row.map_or(anchor.left, |r| r.left);
    if row.is_none() {
        left = match origin {
            Origin::End => anchor.left + anchor.width - width,
            Origin::Anchor => anchor.left + (anchor.width - width) / 2.,
            Origin::Auto if left + width > right => anchor.left + anchor.width - width,
            _ => left,
        };
    }
    left = clamp(left, viewport.left + 16., right - width);
    let below = bottom - (anchor.top + anchor.height + gap);
    let above = anchor.top - gap - viewport.top - 16.;
    let upward = placement == Placement::Auto && desired.1 > below && above > below;
    let height = desired.1.min(if upward { above } else { below }.max(1.));
    let top = if upward {
        anchor.top - gap - height
    } else {
        anchor.top + anchor.height + gap
    };
    let seed_width = anchor.width.min(width);
    Some(Geometry {
        panel: Rect {
            left,
            top,
            width,
            height,
        },
        seed: Rect {
            left: clamp(anchor.left - left, 0., width - seed_width),
            top: if upward { height - 2. } else { 0. },
            width: seed_width,
            height: 2.,
        },
        upward,
    })
}

/// Solve Bezier x(t) before y(t); feeding elapsed time directly to y is wrong.
pub fn expansion_ease(progress: f32) -> f32 {
    let x = progress.clamp(0., 1.);
    let mut low = 0.;
    let mut high = 1.;
    for _ in 0..24 {
        let t = (low + high) / 2.;
        let bx = 3. * (1. - t) * (1. - t) * t * 0.16 + 3. * (1. - t) * t * t * 0.3 + t * t * t;
        if bx < x {
            low = t;
        } else {
            high = t;
        }
    }
    let t = (low + high) / 2.;
    1. - (1. - t).powi(3)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rect(left: f32, top: f32, width: f32, height: f32) -> Rect {
        Rect {
            left,
            top,
            width,
            height,
        }
    }
    #[test]
    fn lab_recorder_seeds_match_all_three_actual_trigger_bounds() {
        let viewport = rect(0., 0., 1280., 720.);
        let row = rect(67.375, 478., 679.1042, 54.);
        for (left, expected_seed) in [(67.375, 0.), (297.7396, 230.3646), (528.1042, 460.7292)] {
            let result = anchored_geometry(
                rect(left, 478., 218.3646, 54.),
                (row.width, 240.),
                viewport,
                Origin::Auto,
                12.,
                Some(row),
                Placement::Below,
            )
            .unwrap();
            assert!(!result.upward);
            assert_eq!(result.panel, rect(67.375, 544., 679.1042, 160.));
            assert!((result.seed.left - expected_seed).abs() < 0.001);
            assert_eq!(result.seed.height, 2.);
        }
    }
    #[test]
    fn lab_menu_flips_up_clamps_and_bounds_scroll_viewport() {
        let result = anchored_geometry(
            rect(1150., 650., 110., 34.),
            (320., 700.),
            rect(0., 0., 1280., 720.),
            Origin::Auto,
            6.,
            None,
            Placement::Auto,
        )
        .unwrap();
        assert!(result.upward);
        assert_eq!(result.panel, rect(940., 16., 320., 628.));
        assert_eq!(result.seed, rect(210., 626., 110., 2.));
        assert!(
            anchored_geometry(
                rect(10., 10., 40., 30.),
                (100., 80.),
                rect(0., 0., 0., 0.),
                Origin::Auto,
                6.,
                None,
                Placement::Auto
            )
            .is_none()
        );
    }
    #[test]
    fn lab_expansion_curve_is_monotonic_without_overshoot() {
        let mut previous = 0.;
        for i in 0..=100 {
            let value = expansion_ease(i as f32 / 100.);
            assert!(value >= previous && value <= 1.);
            previous = value;
        }
        assert!(expansion_ease(0.).abs() < 0.00001);
        assert!((expansion_ease(1.) - 1.).abs() < 0.00001);
        assert!((expansion_ease(0.5) - 0.971779).abs() < 0.0001);
    }
}
