use crate::ui::theme::{colors::Color, typography::FONT_FAMILY};
use gpui::{AnyElement, App, IntoElement, RenderOnce, Window, div, prelude::*, px};
use std::{cell::RefCell, rc::Rc};

/// Bounds are collected during prepaint and consumed during paint in the same
/// frame, so scroll/docking never needs a second render or stale saved geometry.
#[derive(Clone, Default)]
pub(crate) struct PanelBackgrounds(Rc<RefCell<PanelBounds>>);
#[derive(Default)]
struct PanelBounds {
    content: Option<gpui::Bounds<gpui::Pixels>>,
    panels: Vec<(gpui::Bounds<gpui::Pixels>, gpui::Pixels)>,
}
#[cfg(test)]
thread_local! {
    static PAINTED_PANELS: RefCell<Vec<(gpui::Bounds<gpui::Pixels>, gpui::Pixels)>> = const { RefCell::new(Vec::new()) };
}
impl PanelBackgrounds {
    #[cfg(test)]
    pub(crate) fn painted_panels() -> Vec<(gpui::Bounds<gpui::Pixels>, gpui::Pixels)> {
        PAINTED_PANELS.with(|panels| panels.borrow().clone())
    }
    /// One shell fill with holes for panels, shared by Recorder and Settings.
    pub(crate) fn shell(&self, native: Option<gpui::Bounds<gpui::Pixels>>) -> impl IntoElement {
        let regions = self.clone();
        let background = Color::Background.get();
        gpui::canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                let regions = regions.0.borrow();
                #[cfg(test)]
                PAINTED_PANELS.with(|panels| *panels.borrow_mut() = regions.panels.clone());
                if let Some(content) = regions.content {
                    if let Some(path) = background_path(bounds, [(content, px(0.))]) {
                        window.paint_path(path, background);
                    }
                    let holes = native
                        .into_iter()
                        .map(|b| (b, px(0.)))
                        .chain(regions.panels.iter().copied());
                    if let Some(path) = background_path(bounds, holes) {
                        window.with_content_mask(
                            Some(gpui::ContentMask { bounds: content }),
                            |window| window.paint_path(path, background),
                        );
                    }
                } else if let Some(path) =
                    background_path(bounds, native.into_iter().map(|b| (b, px(0.))))
                {
                    window.paint_path(path, background);
                }
            },
        )
        .absolute()
        .inset_0()
        .size_full()
    }

    pub(crate) fn track(&self, radius: Option<gpui::Pixels>) -> impl IntoElement {
        let regions = self.clone();
        gpui::canvas(
            move |bounds, _, _| {
                let mut regions = regions.0.borrow_mut();
                if let Some(radius) = radius {
                    regions.panels.push((bounds, radius));
                } else {
                    regions.content = Some(bounds);
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0()
        .size_full()
    }
}

fn background_path(
    bounds: gpui::Bounds<gpui::Pixels>,
    holes: impl IntoIterator<Item = (gpui::Bounds<gpui::Pixels>, gpui::Pixels)>,
) -> Option<gpui::Path<gpui::Pixels>> {
    let mut path = gpui::PathBuilder::fill().with_style(gpui::PathStyle::Fill(
        gpui::FillOptions::default().with_fill_rule(gpui::FillRule::EvenOdd),
    ));
    for (b, radius) in std::iter::once((bounds, px(0.))).chain(holes) {
        if b.size.width <= px(0.) || b.size.height <= px(0.) {
            continue;
        }
        let r = radius
            .min(b.size.width / 2.)
            .min(b.size.height / 2.)
            .max(px(0.));
        let (l, t, right, bottom) = (b.left(), b.top(), b.right(), b.bottom());
        let point = gpui::point;
        let corner = |path: &mut gpui::PathBuilder, to| {
            if r > px(0.) {
                path.arc_to(point(r, r), px(0.), false, true, to);
            } else {
                path.line_to(to);
            }
        };
        path.move_to(point(l + r, t));
        path.line_to(point(right - r, t));
        corner(&mut path, point(right, t + r));
        path.line_to(point(right, bottom - r));
        corner(&mut path, point(right - r, bottom));
        path.line_to(point(l + r, bottom));
        corner(&mut path, point(l, bottom - r));
        path.line_to(point(l, t + r));
        corner(&mut path, point(l + r, t));
        path.close();
    }
    path.build().ok()
}

/// Preview and transport share the top row; Audio and Display sit side by side below.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RecorderGeometry {
    pub width: f32,
    pub left: f32,
    pub right: f32,
    #[cfg_attr(not(test), allow(dead_code))]
    pub gap: f32,
    pub media: f32,
    #[cfg_attr(not(test), allow(dead_code))]
    pub audio: f32,
    pub row_gap: f32,
}
impl RecorderGeometry {
    pub(crate) fn new(width: f32, height: f32) -> Self {
        let inset = (width * 0.026).clamp(24., 60.);
        let available = (width - inset * 2.).max(640.);
        let gap = (available * 0.024).clamp(16., 40.);
        let right = ((available - gap) * 0.26).clamp(288., 720.);
        let left = (available - right - gap).min(1600.);
        let audio = 232.;
        let row_gap = 28.;
        let media = (left * 9. / 16.).min((height - 64. - row_gap - audio - 32.).max(180.));
        Self {
            width: left + gap + right,
            left,
            right,
            gap,
            media,
            audio,
            row_gap,
        }
    }
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn preview_width(self) -> f32 {
        // Height can be limited by the controls below; shrink both axes together.
        self.media * 16. / 9.
    }
    pub(crate) fn audio_width(self) -> f32 {
        self.left * 0.82
    }
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn display_width(self) -> f32 {
        self.width - self.audio_width() - self.gap
    }
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn shelf_width(self) -> f32 {
        (self.left * 0.46).max(180.)
    }
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn settings_width(self) -> f32 {
        self.left - self.shelf_width() - 12.
    }
    pub(crate) fn inline_transport(self) -> bool {
        self.right >= self.transport_width() + 8. + self.accessory_width()
    }
    pub(crate) fn transport_width(self) -> f32 {
        186.
    }
    pub(crate) fn transport_height(self) -> f32 {
        60.
    }
    pub(crate) fn dock_width(self, _: bool) -> f32 {
        self.transport_height()
    }
    pub(crate) fn wrap_accessories(self) -> bool {
        self.right < self.accessory_width()
    }
    pub(crate) fn accessory_width(self) -> f32 {
        self.dock_width(false) * 4. + 24.
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
    workspace: AnyElement,
    overlays: Vec<AnyElement>,
    messages: Option<AnyElement>,
    native_viewport: Option<gpui::Bounds<gpui::Pixels>>,
    panel_backgrounds: PanelBackgrounds,
}
impl RecorderScreen {
    pub fn new(titlebar: impl IntoElement, workspace: impl IntoElement) -> Self {
        Self {
            titlebar: titlebar.into_any_element(),
            workspace: workspace.into_any_element(),
            overlays: vec![],
            messages: None,
            native_viewport: None,
            panel_backgrounds: PanelBackgrounds::default(),
        }
    }
    pub fn native_viewport(mut self, viewport: Option<gpui::Bounds<gpui::Pixels>>) -> Self {
        self.native_viewport = viewport;
        self
    }
    pub(crate) fn panel_backgrounds(mut self, regions: PanelBackgrounds) -> Self {
        self.panel_backgrounds = regions;
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
impl RenderOnce for RecorderScreen {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let viewport = crate::ui::viewport(window, cx);
        let (width, height) = (f32::from(viewport.width), f32::from(viewport.height));
        let mut root = div()
            .id("recorder-shell")
            .relative()
            .size_full()
            .overflow_hidden()
            .font_family(FONT_FAMILY)
            .text_size(px(15.))
            .line_height(px(22.))
            .text_color(Color::PrimaryText.get());
        // Keep the transparent hole for the existing independent native child HWND.
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
        let frosted = crate::ui::theme::surface_treatment::is_frosted();
        if frosted {
            root = root.child(self.panel_backgrounds.shell(self.native_viewport));
        }
        for (x, y, w, h) in rectangles.into_iter().filter(|_| !frosted) {
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
        root = root
            .child(
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
                            .relative()
                            .overflow_y_scroll()
                            .when(frosted, |d| d.child(self.panel_backgrounds.track(None)))
                            .child(self.workspace),
                    ),
            )
            .when_some(self.messages, |d, m| {
                d.child(
                    div()
                        .id("recorder-messages")
                        .absolute()
                        .bottom(px(8.))
                        .left(px(12.))
                        .right(px(12.))
                        .max_h(px(48.))
                        .overflow_y_scroll()
                        .child(m),
                )
            })
            .children(self.overlays);
        let root = crate::ui::finish_design(
            crate::ui::designable(root, "home", "Recorder workspace", cx)
                .size_full()
                .p_0(),
            "home",
            cx,
        );
        crate::ui::primitives::control_scope::ControlScope::new(
            crate::ui::primitives::control_scope::ControlStyle {
                home: true,
                settings: false,
                inert: false,
                segment_indicator: false,
            },
            root,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn panel_regions_follow_actual_prepaint_and_the_scroll_viewport(cx: &mut gpui::TestAppContext) {
        use crate::ui::theme::{SurfacePreference, ThemeId, surface_treatment};
        use gpui::{Render, size};
        ThemeId::Black.activate();
        SurfacePreference::Frosted.activate();
        crate::ui::theme::set_low_cost_effects(false);
        if !surface_treatment::is_frosted() {
            return;
        }
        struct Screen {
            regions: PanelBackgrounds,
        }
        impl Render for Screen {
            fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
                self.regions = PanelBackgrounds::default();
                RecorderScreen::new(
                    div().h(px(48.)).flex_shrink_0(),
                    div().h(px(1200.)).child(
                        div()
                            .relative()
                            .w(px(400.))
                            .h(px(200.))
                            .child(self.regions.track(Some(px(10.)))),
                    ),
                )
                .panel_backgrounds(self.regions.clone())
            }
        }
        let (view, cx) = cx.add_window_view(|_, _| Screen {
            regions: PanelBackgrounds::default(),
        });
        for height in [640., 400., 900.] {
            cx.simulate_resize(size(px(800.), px(height)));
            cx.run_until_parked();
            cx.update(|_, cx| {
                let regions = view.read(cx).regions.0.borrow();
                let content = regions.content.expect("scroll content was measured");
                assert_eq!(content.top(), px(48.));
                assert_eq!(
                    content.size.height,
                    px(height - 48.),
                    "clip must follow the viewport, not the 1200px child"
                );
                assert_eq!(regions.panels.len(), 1, "one measurement per frame");
                assert_eq!(regions.panels[0].0.size.height, px(200.));
            });
        }
    }

    #[test]
    fn shell_path_leaves_panel_and_native_pixels_unpainted() {
        use gpui::{Bounds, Pixels, point, size};
        let rect = |x, y, w, h| Bounds::new(point(px(x), px(y)), size(px(w), px(h)));
        let root = rect(0., 0., 1000., 800.);
        let native = rect(20., 80., 600., 340.);
        let panels = [
            rect(650., 80., 320., 340.),
            rect(20., 460., 570., 250.),
            rect(620., 460., 350., 250.),
        ];
        let path = background_path(
            root,
            std::iter::once((native, px(0.))).chain(panels.map(|b| (b, px(10.)))),
        )
        .unwrap();
        let covered = |p: gpui::Point<Pixels>| {
            path.vertices.chunks_exact(3).any(|triangle| {
                let cross = |a: gpui::Point<Pixels>, b: gpui::Point<Pixels>| {
                    f32::from(b.x - a.x) * f32::from(p.y - a.y)
                        - f32::from(b.y - a.y) * f32::from(p.x - a.x)
                };
                let a = triangle[0].xy_position;
                let b = triangle[1].xy_position;
                let c = triangle[2].xy_position;
                let sides = [cross(a, b), cross(b, c), cross(c, a)];
                sides.iter().all(|s| *s >= 0.) || sides.iter().all(|s| *s <= 0.)
            })
        };
        assert!(
            !covered(native.center()),
            "native capture must remain unobstructed"
        );
        for b in panels {
            assert!(
                !covered(b.center()),
                "panel must not have a shell fill beneath it"
            );
            assert!(covered(b.origin), "rounded panel corners retain the shell");
        }
        assert!(covered(point(px(500.), px(30.))), "titlebar");
        assert!(covered(point(px(500.), px(440.))), "panel gap");
    }

    #[test]
    fn home_polish_geometry_stays_balanced_across_window_sizes() {
        for (w, h) in [
            (800., 640.),
            (960., 720.),
            (1100., 740.),
            (1280., 720.),
            (1600., 900.),
            (2560., 1440.),
            (3840., 640.),
            (800., 1200.),
        ] {
            let g = RecorderGeometry::new(w, h);
            assert!(g.width <= w - 48. + 0.1, "{g:?}");
            assert!((g.left + g.right + g.gap - g.width).abs() < 0.1);
            assert!(g.left > g.right && g.right >= 288., "{g:?}");
            assert!(64. + g.media + g.row_gap + g.audio <= h, "{g:?}");
            assert!(g.media <= g.left * 9. / 16. + 0.01, "{g:?}");
            assert!(g.preview_width() <= g.left + 0.01, "{g:?}");
            assert!((g.preview_width() / g.media - 16. / 9.).abs() < 0.001);
            assert!(g.settings_width() >= 200., "{g:?}");
            assert!((g.audio_width() / g.left - 0.82).abs() < 0.001);
            assert!((g.audio_width() + g.gap + g.display_width() - g.width).abs() < 0.01);
            assert!(g.display_width() >= 288.);
            assert_eq!(g.audio, 232.);
        }
        let normal = RecorderGeometry::new(1280., 940.);
        assert!(normal.left > 850. && normal.right < 320., "{normal:?}");
        assert!((normal.left / normal.media - 16. / 9.).abs() < 0.001);
    }
}
