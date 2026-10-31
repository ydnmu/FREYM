//! Embedded HWND reuses the bounded GPU handoff. No pixels enter GPUI.
use frame_contracts::{CaptureTarget, RecorderState};
use frame_runtime::recording_control::{PreviewLayers, RecordingControl};
use frame_windows::{
    gpu_preview::{self, GpuPreviewPresenter, GpuPreviewSink},
    preview::PreviewHub,
};
use gpui::{Context, Window, div, prelude::*, px};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

struct Session {
    hub: Arc<PreviewHub>,
    sink: Arc<GpuPreviewSink>,
    presenter: GpuPreviewPresenter,
}
impl Session {
    fn close(mut self) {
        if self
            .hub
            .gpu_sink()
            .as_ref()
            .is_some_and(|s| Arc::ptr_eq(s, &self.sink))
        {
            let _ = self.hub.set_gpu_sink(None);
        }
        self.presenter.close();
    }
}

type AppliedCutouts = Option<((i32, i32), Vec<(i32, i32, i32, i32)>)>;

pub struct Preview {
    control: Arc<RecordingControl>,
    child: Option<isize>,
    session: Option<Session>,
    desired: Option<(CaptureTarget, bool, PreviewLayers)>,
    connected: Option<(CaptureTarget, bool, PreviewLayers)>,
    dimensions: (u32, u32),
    viewport: Option<gpui::Bounds<gpui::Pixels>>,
    clip: Option<gpui::Bounds<gpui::Pixels>>,
    pending: bool,
    closing: bool,
    visible: bool,
    exposed: bool,
    error: Option<String>,
    camera_warning: Option<String>,
    observed_state: RecorderState,
    watcher: Option<gpui::Task<()>>,
    placement: Option<((i32, i32, i32, i32), bool)>,
    activity: crate::preview_activity::PreviewActivity,
    protected: bool,
    first_frame: bool,
    editing: bool,
    cutouts: Vec<gpui::Bounds<gpui::Pixels>>,
    applied_cutouts: AppliedCutouts,
}
impl Preview {
    pub fn new(
        control: Arc<RecordingControl>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let child = gpu_preview::create_child_window(super::home::Home::raw_owner(window));
        let error = child.as_ref().err().cloned();
        let mut this = Self {
            control,
            child: child.ok(),
            session: None,
            desired: None,
            connected: None,
            dimensions: (1, 1),
            viewport: None,
            clip: None,
            pending: false,
            closing: false,
            visible: true,
            exposed: false,
            error,
            camera_warning: None,
            observed_state: RecorderState::Idle,
            watcher: None,
            placement: None,
            activity: Default::default(),
            protected: true,
            first_frame: false,
            editing: false,
            cutouts: Vec::new(),
            applied_cutouts: None,
        };
        cx.observe_window_visibility(window, |this, visibility, window, cx| {
            this.visible = visibility.is_visible();
            this.place(window);
            this.refresh(window, cx);
        })
        .detach();
        this.watcher = Some(cx.spawn_in(window, async move |view, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                if view
                    .update_in(cx, |this, window, cx| this.refresh(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
        this
    }
    pub fn configure(
        &mut self,
        desired: Option<(CaptureTarget, bool, PreviewLayers)>,
        dimensions: (u32, u32),
        exposed: bool,
        protected: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.desired != desired {
            self.activity.interact(Instant::now());
            self.desired = desired;
            self.dimensions = dimensions;
            self.error = None;
        }
        self.exposed = exposed;
        self.protected = protected;
        self.place(window);
        self.refresh(window, cx);
    }
    pub fn camera_warning(&self) -> Option<&str> {
        self.camera_warning.as_deref()
    }
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    pub fn set_editing(&mut self, editing: bool) {
        self.editing = editing;
        if !editing {
            self.cutouts.clear();
        }
    }
    pub fn set_cutouts(&mut self, cutouts: Vec<gpui::Bounds<gpui::Pixels>>, window: &mut Window) {
        if self.editing && self.cutouts != cutouts {
            self.cutouts = cutouts;
            self.place(window);
        }
    }
    pub fn sleeping(&self) -> bool {
        self.activity.sleeping
    }
    pub fn ready(&self) -> bool {
        self.first_frame
    }
    pub fn note_interaction(&mut self) {
        if !self.activity.sleeping {
            self.activity.interact(Instant::now());
        }
    }
    pub fn interact(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let wake = self.activity.interact(Instant::now());
        let retry = self.error.is_some();
        if retry {
            self.retry();
        }
        if wake || retry {
            self.refresh(window, cx);
            cx.notify();
        }
    }
    pub fn retry(&mut self) {
        self.error = None;
        self.connected = None;
    }
    pub fn hole(&self, scale: f32) -> Option<gpui::Bounds<gpui::Pixels>> {
        if self.session.is_none() || self.error.is_some() || !self.exposed || self.closing {
            return None;
        }
        let b = self.viewport?;
        let source = self
            .session
            .as_ref()
            .and_then(|s| s.sink.dimensions())
            .unwrap_or(self.dimensions);
        let (x, y, w, h) = fit_rect(
            (
                b.left().into(),
                b.top().into(),
                b.size.width.into(),
                b.size.height.into(),
            ),
            scale,
            source,
        )
        .ok()?;
        let fitted = gpui::Bounds::new(
            gpui::point(px(x as f32 / scale), px(y as f32 / scale)),
            gpui::size(px(w as f32 / scale), px(h as f32 / scale)),
        );
        // A native child cannot inherit GPUI's scroll mask. Hide it while
        // partially scrolled out rather than painting over another panel/header.
        self.clip
            .filter(|clip| contains_rect(*clip, fitted))
            .map(|_| fitted)
    }
    pub fn is_closed(&self) -> bool {
        self.closing && !self.pending && self.child.is_none()
    }
    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending || self.closing || self.child.is_none() {
            return;
        }
        let state = self.control.published().state;
        let protected = self.protected
            || state != RecorderState::Idle
            || self.pending
            || !self.exposed
            || !self.visible;
        if self.activity.update(Instant::now(), protected) {
            cx.notify();
        }
        let ready = self
            .session
            .as_ref()
            .is_some_and(|s| s.sink.stats().presented > 0);
        if self.first_frame != ready {
            self.first_frame = ready;
            cx.notify();
        }
        if state != self.observed_state {
            self.error = None;
            self.observed_state = state.clone();
        }
        let wanted = if self.visible && !(self.activity.sleeping && state == RecorderState::Idle) {
            self.desired.clone()
        } else {
            None
        };
        let hub = self.control.preview_hub();
        let camera_warning = hub.as_ref().and_then(|h| h.camera_warning());
        if self.camera_warning != camera_warning {
            self.camera_warning = camera_warning;
            cx.notify();
        }
        if let Some(dimensions) = self.session.as_ref().and_then(|s| s.sink.dimensions())
            && dimensions != self.dimensions
        {
            self.dimensions = dimensions;
            self.place(window);
            cx.notify();
        }
        if let Some(error) = self.session.as_ref().and_then(|s| s.sink.error())
            && self.error.as_ref() != Some(&error)
        {
            self.error = Some(error);
            self.place(window);
            cx.notify();
        }
        let changed = self.connected != wanted
            || self.session.as_ref().is_some_and(|s| {
                hub.as_ref()
                    .is_none_or(|h| h.generation() != s.hub.generation())
            });
        // A hidden recording can finish and restore its remembered idle capture.
        // Retire that owner too, without resuming GPU work in a hidden window.
        let hidden_idle = wanted.is_none() && state == RecorderState::Idle && hub.is_some();
        if !changed
            && !hidden_idle
            && (self.session.is_some() || wanted.is_none() || self.error.is_some())
        {
            return;
        }
        // Preparing/finalizing replaces the capture hub. Wait for stable publication.
        if !matches!(
            state,
            RecorderState::Idle | RecorderState::Recording | RecorderState::Paused
        ) {
            self.place(window);
            return;
        }
        // Placement-only edits keep the presenter and WGC owners alive. The
        // single in-flight command coalesces pointer moves to the latest layout.
        let placement_only = state == RecorderState::Idle
            && self.session.is_some()
            && self
                .connected
                .as_ref()
                .zip(wanted.as_ref())
                .is_some_and(|(a, b)| a.0 == b.0 && a.1 == b.1 && a.2.same_sources(&b.2))
            && self.session.as_ref().is_some_and(|s| {
                hub.as_ref()
                    .is_some_and(|h| h.generation() == s.hub.generation())
            });
        if placement_only {
            let (target, cursor, layers) = wanted.clone().unwrap();
            self.connected = wanted;
            self.pending = true;
            let control = self.control.clone();
            let task = cx
                .background_executor()
                .spawn(async move { control.open_gpu_preview_with_layers(target, cursor, layers) });
            cx.spawn_in(window, async move |view, cx| {
                let result = task.await;
                let _ = view.update_in(cx, |this, window, cx| {
                    this.pending = false;
                    if let Err(e) = result {
                        this.error = Some(format!("{} {}", e.message, e.action));
                    }
                    if this.closing {
                        this.close(window, cx);
                    } else {
                        this.refresh(window, cx);
                        cx.notify();
                    }
                });
            })
            .detach();
            return;
        }
        self.pending = true;
        let old = self.session.take();
        self.connected = wanted.clone();
        self.place(window);
        let control = self.control.clone();
        let hwnd = self.child.unwrap();
        let task = cx.background_executor().spawn(async move {
            if let Some(s) = old {
                s.close();
            }
            let Some((target, cursor, layers)) = wanted else {
                control
                    .close_preview()
                    .map_err(|e| format!("{} {}", e.message, e.action))?;
                return Ok(None);
            };
            if control.published().state == RecorderState::Idle {
                control
                    .open_gpu_preview_with_layers(target, cursor, layers)
                    .map_err(|e| format!("{} {}", e.message, e.action))?;
            }
            let hub = control
                .preview_hub()
                .ok_or("The active source has no native GPU preview.")?;
            let sink = hub
                .gpu_sink()
                .filter(|s| s.active())
                .unwrap_or_else(|| GpuPreviewSink::new(hub.generation()));
            sink.set_visible(false);
            if hub
                .gpu_sink()
                .as_ref()
                .is_none_or(|s| !Arc::ptr_eq(s, &sink))
            {
                hub.set_gpu_sink(Some(sink.clone()))?;
            }
            match GpuPreviewPresenter::start(&sink, hwnd) {
                Ok(presenter) => Ok(Some(Session {
                    hub,
                    sink,
                    presenter,
                })),
                Err(e) => {
                    let _ = hub.set_gpu_sink(None);
                    Err(e)
                }
            }
        });
        cx.spawn_in(window, async move |view, cx| {
            let result = task.await;
            // The entity is retained by Home until all presenter joins complete.
            let _ = view.update_in(cx, |this, window, cx| {
                this.pending = false;
                match result {
                    Ok(s) => {
                        this.session = s;
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e),
                }
                if this.closing {
                    this.close(window, cx);
                } else {
                    this.place(window);
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }
    pub fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.closing = true;
        self.watcher = None;
        self.place(window);
        if self.pending || self.child.is_none() {
            return;
        }
        self.pending = true;
        let session = self.session.take();
        let control = self.control.clone();
        let task = cx.background_executor().spawn(async move {
            if let Some(s) = session {
                s.close();
            }
            let _ = control.close_preview();
        });
        cx.spawn_in(window, async move |view, cx| {
            task.await;
            let _ = view.update_in(cx, |this, _, cx| {
                this.pending = false;
                if let Some(hwnd) = this.child.take() {
                    let _ = gpu_preview::destroy_child_window(hwnd);
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn place(&mut self, window: &mut Window) {
        let hole = self.hole(window.scale_factor());
        let visible = self.visible && hole.is_some();
        if let Some(s) = &self.session {
            s.sink.set_visible(visible);
        }
        if let Some(hwnd) = self.child {
            let b = hole.unwrap_or(gpui::Bounds::new(
                gpui::point(px(0.), px(0.)),
                gpui::size(px(1.), px(1.)),
            ));
            let scale = window.scale_factor();
            let rect = (
                (f32::from(b.left()) * scale).round() as i32,
                (f32::from(b.top()) * scale).round() as i32,
                (f32::from(b.size.width) * scale).round() as i32,
                (f32::from(b.size.height) * scale).round() as i32,
            );
            let cutouts: Vec<_> = self
                .cutouts
                .iter()
                .map(|c| {
                    (
                        ((f32::from(c.left() - b.left())) * scale).floor() as i32,
                        ((f32::from(c.top() - b.top())) * scale).floor() as i32,
                        ((f32::from(c.right() - b.left())) * scale).ceil() as i32,
                        ((f32::from(c.bottom() - b.top())) * scale).ceil() as i32,
                    )
                })
                .collect();
            let mask = ((rect.2, rect.3), cutouts);
            if self.applied_cutouts.as_ref() != Some(&mask) {
                if let Err(e) = gpu_preview::set_child_window_cutouts(hwnd, mask.0, &mask.1) {
                    self.error = Some(e);
                    return;
                }
                self.applied_cutouts = Some(mask);
            }
            if self.placement == Some((rect, visible)) {
                return;
            }
            if let Err(e) = gpu_preview::place_child_window(hwnd, rect, visible) {
                self.error = Some(e);
            } else {
                self.placement = Some((rect, visible));
            }
        }
    }
}
impl Render for Preview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.weak_entity();
        div().size_full().child(
            gpui::canvas(
                |_, _, _| (),
                move |bounds, _, window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        let clip = window.content_mask().bounds;
                        if this.viewport != Some(bounds) || this.clip != Some(clip) {
                            this.viewport = Some(bounds);
                            this.clip = Some(clip);
                            cx.notify();
                        }
                        this.place(window);
                        if let Some(hole) = this.hole(window.scale_factor()) {
                            for rect in [
                                gpui::Bounds::new(
                                    bounds.origin,
                                    gpui::size(bounds.size.width, hole.top() - bounds.top()),
                                ),
                                gpui::Bounds::new(
                                    gpui::point(bounds.left(), hole.bottom()),
                                    gpui::size(bounds.size.width, bounds.bottom() - hole.bottom()),
                                ),
                                gpui::Bounds::new(
                                    gpui::point(bounds.left(), hole.top()),
                                    gpui::size(hole.left() - bounds.left(), hole.size.height),
                                ),
                                gpui::Bounds::new(
                                    gpui::point(hole.right(), hole.top()),
                                    gpui::size(bounds.right() - hole.right(), hole.size.height),
                                ),
                            ] {
                                window.paint_quad(gpui::fill(
                                    rect,
                                    crate::ui::theme::colors::Color::PreviewSurface.get(),
                                ));
                            }
                        }
                    });
                },
            )
            .size_full(),
        )
    }
}
fn contains_rect(outer: gpui::Bounds<gpui::Pixels>, inner: gpui::Bounds<gpui::Pixels>) -> bool {
    // Native coordinates round to physical pixels; a subpixel difference must
    // not suppress the entire child at fractional Windows DPI.
    let tolerance = px(1.);
    inner.left() + tolerance >= outer.left()
        && inner.right() <= outer.right() + tolerance
        && inner.top() + tolerance >= outer.top()
        && inner.bottom() <= outer.bottom() + tolerance
}
pub fn fit_rect(
    rect: (f32, f32, f32, f32),
    scale: f32,
    source: (u32, u32),
) -> Result<(i32, i32, i32, i32), String> {
    let (mut x, mut y, mut w, mut h) = rect;
    if ![x, y, w, h, scale].iter().all(|v| v.is_finite())
        || scale <= 0.
        || w < 2.
        || h < 2.
        || source.0 == 0
        || source.1 == 0
    {
        return Err("Preview viewport is invalid.".into());
    }
    let aspect = source.0 as f32 / source.1 as f32;
    if w / h > aspect {
        let fitted = h * aspect;
        x += (w - fitted) / 2.;
        w = fitted;
    } else {
        let fitted = w / aspect;
        y += (h - fitted) / 2.;
        h = fitted;
    }
    if [x, y, w, h]
        .iter()
        .any(|v| (*v * scale).abs() > i32::MAX as f32)
    {
        return Err("Preview viewport overflowed.".into());
    }
    Ok((
        (x * scale).round() as i32,
        (y * scale).round() as i32,
        (w * scale).round() as i32,
        (h * scale).round() as i32,
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn viewport_fits_source_without_stretch_at_mixed_dpi() {
        assert_eq!(
            fit_rect((0., 0., 800., 600.), 1.25, (1920, 1080)).unwrap(),
            (0, 94, 1000, 563)
        );
        assert!(fit_rect((0., 0., 800., 600.), f32::NAN, (1920, 1080)).is_err());
    }
    #[test]
    fn native_child_never_escapes_a_scroll_mask() {
        let bounds =
            |x, y, w, h| gpui::Bounds::new(gpui::point(px(x), px(y)), gpui::size(px(w), px(h)));
        let mask = bounds(0., 48., 800., 592.);
        assert!(contains_rect(mask, bounds(10., 60., 640., 360.)));
        assert!(!contains_rect(mask, bounds(10., 20., 640., 360.)));
        assert!(!contains_rect(mask, bounds(700., 60., 640., 360.)));
        assert!(
            contains_rect(mask, bounds(-0.4, 48., 800.8, 592.4)),
            "fractional DPI rounding must not hide the entire preview"
        );
    }
}
