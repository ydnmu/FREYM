//! Native full-display GPUI area overlay. Coordinates are local physical pixels.
use crate::ui::{
    primitives::{Surface, Text},
    theme::{colors::Color, spacing::Spacing},
};
use frame_contracts::{DisplayInfo, Region};
use gpui::{
    Context, FocusHandle, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    Window, div, point, prelude::*, px,
};
type OnRegionSelected = Box<dyn FnOnce(Option<Region>, &mut gpui::App)>;
pub struct RegionPicker {
    display: DisplayInfo,
    focus: FocusHandle,
    start: Option<Point<Pixels>>,
    end: Point<Pixels>,
    done: Option<OnRegionSelected>,
}
impl RegionPicker {
    pub fn new(
        display: DisplayInfo,
        done: impl FnOnce(Option<Region>, &mut gpui::App) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        window.set_window_title("FREYM area selection");
        freym_platform::set_selection_overlay(crate::home::Home::raw_owner(window));
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let view = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            let _ = view.update(cx, |this, cx| this.finish(None, window, cx));
            false
        });
        Self {
            display,
            focus,
            start: None,
            end: point(px(0.), px(0.)),
            done: Some(Box::new(done)),
        }
    }
    fn finish(&mut self, region: Option<Region>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(done) = self.done.take() {
            done(region, cx);
        }
        window.remove_window();
    }
}
pub fn physical_region(
    display: &DisplayInfo,
    start: (f32, f32),
    end: (f32, f32),
    scale: f32,
) -> Option<Region> {
    if !scale.is_finite()
        || scale <= 0.
        || ![start.0, start.1, end.0, end.1]
            .iter()
            .all(|v| v.is_finite())
    {
        return None;
    }
    let x = (start.0.min(end.0) * scale)
        .round()
        .clamp(0., display.width as f32) as i32;
    let y = (start.1.min(end.1) * scale)
        .round()
        .clamp(0., display.height as f32) as i32;
    let right = (start.0.max(end.0) * scale)
        .round()
        .clamp(0., display.width as f32) as i32;
    let bottom = (start.1.max(end.1) * scale)
        .round()
        .clamp(0., display.height as f32) as i32;
    if right - x < 2 || bottom - y < 2 {
        return None;
    }
    Some(Region {
        display_id: display.id.clone(),
        x,
        y,
        width: (right - x) as u32 & !1,
        height: (bottom - y) as u32 & !1,
    })
}
impl Render for RegionPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div()
            .id("region-picker")
            .track_focus(&self.focus)
            .key_context("FREYM")
            .size_full()
            .relative()
            .bg(Color::CaptureScrim.get())
            .cursor_crosshair()
            .on_action(cx.listener(|this, _: &crate::home::Cancel, w, cx| this.finish(None, w, cx)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, _, cx| {
                    this.start = Some(e.position);
                    this.end = e.position;
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                if this.start.is_some() {
                    this.end = e.position;
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, e: &MouseUpEvent, w, cx| {
                    if let Some(start) = this.start {
                        let r = physical_region(
                            &this.display,
                            (start.x.into(), start.y.into()),
                            (e.position.x.into(), e.position.y.into()),
                            w.scale_factor(),
                        );
                        this.finish(r, w, cx);
                    }
                }),
            )
            .child(
                div()
                    .absolute()
                    .top(Spacing::Xl.px())
                    .left(Spacing::Xl.px())
                    .child(
                        Surface::new().child(Text::new("Drag to choose an area · Esc to cancel")),
                    ),
            );
        if let Some(start) = self.start {
            let x = start.x.min(self.end.x);
            let y = start.y.min(self.end.y);
            let width = (start.x - self.end.x).abs();
            let height = (start.y - self.end.y).abs();
            root = root.child(
                div()
                    .absolute()
                    .left(x)
                    .top(y)
                    .w(width)
                    .h(height)
                    .border_2()
                    .border_color(Color::OnAccent.get())
                    .bg(Color::CaptureSelection.get()),
            );
        }
        root
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn region_is_local_physical_and_clamped_at_mixed_dpi() {
        let d = DisplayInfo {
            id: frame_contracts::DisplayId("left".into()),
            name: "Left".into(),
            width: 2560,
            height: 1440,
            x: -2560,
            y: 0,
            scale_factor: 1.25,
            refresh_rate_hz: Some(144.),
            is_primary: false,
            hdr_available: false,
            required_recording_quality: None,
        };
        let r = physical_region(&d, (200., 100.), (10., 20.), 1.25).unwrap();
        assert_eq!((r.x, r.y, r.width, r.height), (13, 25, 236, 100));
        assert!(frame_settings::validate_region(&r, &d).is_ok());
        assert!(physical_region(&d, (0., 0.), (0., 0.), 1.25).is_none());
    }
}
