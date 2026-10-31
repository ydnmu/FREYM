//! Fractional decorative motion. Pinned GPUI snaps both layout offsets and quads;
//! paths retain subpixel geometry. Hit areas and layout remain ordinary elements.
use gpui::{Background, Bounds, Path, PathBuilder, Pixels, Style, Window, point, px};

fn rounded_path(
    bounds: Bounds<Pixels>,
    radius: Pixels,
    stroke: Option<Pixels>,
) -> Option<Path<Pixels>> {
    let r = radius
        .min(bounds.size.width / 2.)
        .min(bounds.size.height / 2.)
        .max(px(0.));
    let mut p = stroke
        .map(PathBuilder::stroke)
        .unwrap_or_else(PathBuilder::fill);
    let (l, t, right, bottom) = (bounds.left(), bounds.top(), bounds.right(), bounds.bottom());
    let corner = |p: &mut PathBuilder, to| {
        if r > px(0.) {
            p.arc_to(point(r, r), px(0.), false, true, to);
        } else {
            p.line_to(to);
        }
    };
    p.move_to(point(l + r, t));
    p.line_to(point(right - r, t));
    corner(&mut p, point(right, t + r));
    p.line_to(point(right, bottom - r));
    corner(&mut p, point(right - r, bottom));
    p.line_to(point(l + r, bottom));
    corner(&mut p, point(l, bottom - r));
    p.line_to(point(l, t + r));
    corner(&mut p, point(l + r, t));
    p.close();
    p.build().ok()
}

pub(crate) fn paint(label: &str, bounds: Bounds<Pixels>, style: &Style, window: &mut Window) {
    #[cfg(not(test))]
    let _ = label;
    #[cfg(test)]
    BOUNDS.with(|b| {
        b.borrow_mut().insert(label.to_owned(), bounds);
    });
    let radii = style
        .corner_radii
        .to_pixels(window.rem_size())
        .clamp_radii_for_quad_size(bounds.size);
    window.paint_drop_shadows(bounds, radii, &style.box_shadow);
    if let Some(background) = style.background.as_ref().and_then(gpui::Fill::color)
        && let Some(path) = rounded_path(bounds, radii.top_left, None)
    {
        window.paint_path(path, background);
    }
    window.paint_inset_shadows(bounds, radii, &style.box_shadow);
    let border = style.border_widths.left.to_pixels(window.rem_size());
    if border > px(0.)
        && let Some(color) = style.border_color
        && let Some(path) = rounded_path(
            bounds.dilate(-border / 2.),
            (radii.top_left - border / 2.).max(px(0.)),
            Some(border),
        )
    {
        window.paint_path(path, Background::from(color));
    }
}

#[cfg(test)]
thread_local! { static BOUNDS: std::cell::RefCell<std::collections::HashMap<String,Bounds<Pixels>>> = Default::default(); }
#[cfg(test)]
pub(crate) fn test_bounds(label: &str) -> Bounds<Pixels> {
    BOUNDS.with(|b| b.borrow()[label])
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Context, IntoElement, Render, TestAppContext, canvas, div, prelude::*};
    struct Fixture;
    impl Render for Fixture {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().relative().child(
                canvas(
                    |_, _, _| {},
                    |mut b, _, w, _| {
                        b.origin.x += px(3.25);
                        paint("fractional-thumb", b, &Style::default(), w);
                    },
                )
                .absolute()
                .left(px(2.))
                .size(px(16.)),
            )
        }
    }
    #[gpui::test]
    fn interaction_motion_preserves_subpixel_position_after_layout(cx: &mut TestAppContext) {
        let (_, cx) = cx.add_window_view(|_, _| Fixture);
        cx.run_until_parked();
        assert_eq!(test_bounds("fractional-thumb").left(), px(5.25));
    }
}
