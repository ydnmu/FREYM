use super::*;
use crate::state::workspace::Rect;

#[derive(Default)]
pub(super) struct CameraUi {
    pub loaded: bool,
    pub loading: bool,
    pub settings_supported: bool,
    pub editing: bool,
    pub invalid_format: bool,
    generation: u64,
    pub drag: Option<(gpui::Point<gpui::Pixels>, CameraPlacement, u8)>,
}

pub(super) fn camera_settings_uri(id: &str) -> String {
    let mut escaped = String::new();
    for byte in id.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            escaped.push(byte as char);
        } else {
            use std::fmt::Write;
            let _ = write!(escaped, "%{byte:02X}");
        }
    }
    format!("ms-settings:camera?cameraId={escaped}")
}

// All guide geometry is in fitted-preview logical pixels; values stay in output pixels.
#[derive(Clone, Copy, Debug)]
struct GuideRect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}
impl GuideRect {
    fn overlaps(self, other: Self) -> bool {
        self.x < other.x + other.width + 3.
            && other.x < self.x + self.width + 3.
            && self.y < other.y + other.height + 3.
            && other.y < self.y + self.height + 3.
    }
}
struct DistanceGuide {
    edge: &'static str,
    value: u32,
    start: (f32, f32),
    end: (f32, f32),
    label: Option<GuideRect>,
    compact: bool,
}
struct PlacementGuides {
    handles: [GuideRect; 4],
    done: GuideRect,
    dimensions: Option<GuideRect>,
    distances: [DistanceGuide; 4],
}
fn guide_slot(
    size: (f32, f32),
    preferred: (f32, f32),
    viewport: (f32, f32),
    occupied: &[GuideRect],
) -> Option<GuideRect> {
    let (w, h) = size;
    let (vw, vh) = viewport;
    if vw < w + 8. || vh < h + 8. {
        return None;
    }
    let fit = |x: f32, y: f32| GuideRect {
        x: x.clamp(4., vw - w - 4.),
        y: y.clamp(4., vh - h - 4.),
        width: w,
        height: h,
    };
    let preferred = fit(preferred.0 - w / 2., preferred.1 - h / 2.);
    if !occupied.iter().any(|r| r.overlaps(preferred)) {
        return Some(preferred);
    }
    // A small deterministic grid gives cramped/edge placements a readable readout.
    // It never pushes a badge over another label or a resize target.
    let xs = [preferred.x, 4., vw - w - 4., (vw - w) / 2.];
    let rows = ((vh - 8.) / (h + 4.)).floor() as usize;
    let mut best: Option<(f32, GuideRect)> = None;
    for x in xs {
        // Include the clamped bottom edge when a full grid row does not fit.
        for row in 0..=rows {
            let r = fit(x, 4. + row as f32 * (h + 4.));
            if occupied.iter().any(|o| o.overlaps(r)) {
                continue;
            }
            let distance = (r.x - preferred.x).powi(2) + (r.y - preferred.y).powi(2);
            if best.is_none_or(|(d, _)| distance < d) {
                best = Some((distance, r));
            }
        }
    }
    best.map(|(_, r)| r)
}
fn placement_guides(
    p: CameraPlacement,
    source: (u32, u32),
    picture: Rect,
    viewport: (f32, f32),
) -> PlacementGuides {
    let (vw, vh) = viewport;
    let sx = picture.width / source.0.max(1) as f32;
    let sy = picture.height / source.1.max(1) as f32;
    let (left, top) = (picture.x + p.x as f32 * sx, picture.y + p.y as f32 * sy);
    let (right, bottom) = (
        picture.x + (p.x as f32 + p.width as f32) * sx,
        picture.y + (p.y as f32 + p.height as f32) * sy,
    );
    let (mx, my) = ((left + right) / 2., (top + bottom) / 2.);
    let handle = 16_f32
        .min((right - left) / 2.)
        .min((bottom - top) / 2.)
        .max(0.);
    let handles =
        [(right, bottom), (left, bottom), (left, top), (right, top)].map(|(x, y)| GuideRect {
            x: (x - handle / 2.).clamp(0., (vw - handle).max(0.)),
            y: (y - handle / 2.).clamp(0., (vh - handle).max(0.)),
            width: handle,
            height: handle,
        });
    let mut occupied = handles.to_vec();
    let done = guide_slot((64., 32.), (vw - 36., 20.), viewport, &occupied).unwrap_or(GuideRect {
        x: 4.,
        y: 4.,
        width: 64.,
        height: 32.,
    });
    occupied.push(done);
    let mut distances = [
        DistanceGuide {
            edge: "left",
            value: p.x,
            start: (picture.x, my),
            end: (left, my),
            label: None,
            compact: false,
        },
        DistanceGuide {
            edge: "top",
            value: p.y,
            start: (mx, picture.y),
            end: (mx, top),
            label: None,
            compact: false,
        },
        DistanceGuide {
            edge: "right",
            value: source.0.saturating_sub(p.x.saturating_add(p.width)),
            start: (right, my),
            end: (picture.x + picture.width, my),
            label: None,
            compact: false,
        },
        DistanceGuide {
            edge: "bottom",
            value: source.1.saturating_sub(p.y.saturating_add(p.height)),
            start: (mx, bottom),
            end: (mx, picture.y + picture.height),
            label: None,
            compact: false,
        },
    ];
    // Equal-sized midpoint badges align both opposing pairs.
    let value_width = distances
        .iter()
        .map(|d| format!("{} px", d.value).len())
        .max()
        .unwrap_or(0) as f32
        * 6.5
        + 12.;
    for d in &mut distances {
        let w = value_width;
        let (x, y) = ((d.start.0 + d.end.0) / 2., (d.start.1 + d.end.1) / 2.);
        let r = GuideRect {
            x: x - w / 2.,
            y: y - 9.,
            width: w,
            height: 18.,
        };
        let horizontal = d.start.1 == d.end.1;
        let fits_gap = if horizontal {
            d.end.0 - d.start.0 >= w + 12.
        } else {
            d.end.1 - d.start.1 >= 30.
        };
        if fits_gap
            && r.x >= 4.
            && r.y >= 4.
            && r.x + w <= vw - 4.
            && r.y + 18. <= vh - 4.
            && !occupied.iter().any(|o| o.overlaps(r))
        {
            d.label = Some(r);
            occupied.push(r);
        }
    }
    if distances.iter().any(|d| d.label.is_none()) {
        // Cramped/edge placements use one aligned readout instead of scattered
        // fallback badges. Rules stay behind opaque labels; handles stay clear.
        occupied.truncate(handles.len() + 1);
        let cell_width = value_width + 16.;
        let columns = if vw >= cell_width * 2. + 12. { 2 } else { 1 };
        let group_width = cell_width * columns as f32 + (columns - 1) as f32 * 4.;
        let group_height = (4 / columns) as f32 * 22. - 4.;
        let group = guide_slot(
            (group_width, group_height),
            (mx, top - group_height / 2. - 8.),
            viewport,
            &occupied,
        );
        // The two-column order pairs left/right, then top/bottom.
        for (slot, index) in [0, 2, 1, 3].into_iter().enumerate() {
            let d = &mut distances[index];
            d.compact = true;
            d.label = group.map(|r| GuideRect {
                x: r.x + (slot % columns) as f32 * (cell_width + 4.),
                y: r.y + (slot / columns) as f32 * 22.,
                width: cell_width,
                height: 18.,
            });
        }
        if let Some(r) = group {
            occupied.push(r);
        }
    }
    let dimensions = guide_slot(
        (
            format!("{} × {}", p.width, p.height).chars().count() as f32 * 7. + 12.,
            18.,
        ),
        (mx, my),
        viewport,
        &occupied,
    );
    PlacementGuides {
        handles,
        done,
        dimensions,
        distances,
    }
}

impl Home {
    pub(super) fn camera_editor(
        &self,
        picture: Rect,
        width: f32,
        height: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::ui::{primitives::*, theme::colors::Color};
        let Some(request) = self
            .model
            .request
            .as_ref()
            .filter(|_| self.camera_ui.editing && self.model.editable())
        else {
            return gpui::div().into_any_element();
        };
        let Some(camera) = request.camera.as_ref() else {
            return gpui::div().into_any_element();
        };
        let p = camera.placement;
        let Some((sw, sh)) = self.model.source_dimensions() else {
            return gpui::div().into_any_element();
        };
        if sw == 0 || sh == 0 || width <= 0. || height <= 0. {
            return gpui::div().into_any_element();
        }
        let sx = picture.width / sw as f32;
        let sy = picture.height / sh as f32;
        let guides = placement_guides(p, (sw, sh), picture, (width, height));
        #[cfg(target_os = "windows")]
        let native_cutouts = {
            let (x, y, w, h) = (
                picture.x + p.x as f32 * sx,
                picture.y + p.y as f32 * sy,
                p.width as f32 * sx,
                p.height as f32 * sy,
            );
            let mut rectangles = vec![
                GuideRect {
                    x,
                    y,
                    width: w,
                    height: 1.,
                },
                GuideRect {
                    x,
                    y: y + h - 1.,
                    width: w,
                    height: 1.,
                },
                GuideRect {
                    x,
                    y,
                    width: 1.,
                    height: h,
                },
                GuideRect {
                    x: x + w - 1.,
                    y,
                    width: 1.,
                    height: h,
                },
                guides.done,
            ];
            rectangles.extend(guides.dimensions);
            for d in &guides.distances {
                rectangles.extend(d.label);
                rectangles.push(GuideRect {
                    x: d.start.0,
                    y: d.start.1,
                    width: (d.end.0 - d.start.0).max(1.),
                    height: (d.end.1 - d.start.1).max(1.),
                });
            }
            for ((x, y), r) in [
                (p.x + p.width, p.y + p.height),
                (p.x, p.y + p.height),
                (p.x, p.y),
                (p.x + p.width, p.y),
            ]
            .into_iter()
            .zip(guides.handles)
            {
                let marker = 6_f32.min(r.width);
                rectangles.push(GuideRect {
                    x: (picture.x + x as f32 * sx - marker / 2.)
                        .clamp(0., (width - marker).max(0.)),
                    y: (picture.y + y as f32 * sy - marker / 2.)
                        .clamp(0., (height - marker).max(0.)),
                    width: marker,
                    height: marker,
                });
            }
            let preview = self.preview.clone();
            gpui::canvas(
                |_, _, _| (),
                move |bounds, _, window, cx| {
                    if let Some(preview) = &preview {
                        let cutouts = rectangles
                            .iter()
                            .map(|r| {
                                gpui::Bounds::new(
                                    bounds.origin + gpui::point(px(r.x), px(r.y)),
                                    gpui::size(px(r.width), px(r.height)),
                                )
                            })
                            .collect();
                        preview.update(cx, |p, _| p.set_cutouts(cutouts, window));
                    }
                },
            )
            .absolute()
            .size_full()
        };

        gpui::div()
            .id("camera-preview-editor")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .when(cfg!(target_os = "windows"), |d| {
                #[cfg(target_os = "windows")]
                {
                    d.child(native_cutouts)
                }
                #[cfg(not(target_os = "windows"))]
                {
                    d
                }
            })
            .on_mouse_move(
                cx.listener(move |home, event: &gpui::MouseMoveEvent, _, cx| {
                    if let Some((origin, initial, resize)) = home.camera_ui.drag {
                        let dx = f32::from(event.position.x - origin.x) / sx;
                        let dy = f32::from(event.position.y - origin.y) / sy;
                        let placement = if resize == 0 {
                            moved_placement(initial, dx, dy, false, (sw, sh))
                        } else {
                            corner_placement(initial, dx, dy, resize, (sw, sh))
                        };
                        if let Some(camera) =
                            home.model.request.as_mut().and_then(|r| r.camera.as_mut())
                        {
                            camera.placement = placement;
                        }
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|home, _, _, cx| {
                    if home.camera_ui.drag.take().is_some() {
                        home.persist(cx);
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up_out(
                gpui::MouseButton::Left,
                cx.listener(|home, _, _, cx| {
                    if home.camera_ui.drag.take().is_some() {
                        home.persist(cx);
                        cx.notify();
                    }
                }),
            )
            .child(
                gpui::div()
                    .id("camera-placement-box")
                    .debug_selector(|| "camera-placement-box".into())
                    .absolute()
                    .left(px(picture.x + p.x as f32 * sx))
                    .top(px(picture.y + p.y as f32 * sy))
                    .w(px(p.width as f32 * sx))
                    .h(px(p.height as f32 * sy))
                    .border_1()
                    .border_color(Color::Accent.get())
                    .cursor(if self.camera_ui.drag.is_some() {
                        gpui::CursorStyle::ClosedHand
                    } else {
                        gpui::CursorStyle::OpenHand
                    })
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |home, event: &gpui::MouseDownEvent, _, cx| {
                            home.camera_ui.drag = Some((event.position, p, 0));
                            cx.stop_propagation();
                        }),
                    ),
            )
            // Rules are GPU quads behind opaque badges; no path tessellation or polling.
            .children(guides.distances.iter().map(|d| {
                gpui::div()
                    .absolute()
                    .left(px(d.start.0))
                    .top(px(d.start.1))
                    .w(px((d.end.0 - d.start.0).max(1.)))
                    .h(px((d.end.1 - d.start.1).max(1.)))
                    .bg(Color::Accent.get().opacity(0.35))
            }))
            .children(guides.distances.iter().filter_map(|d| {
                d.label.map(|r| {
                    let id = format!("camera-distance-{}", d.edge);
                    gpui::div()
                        .id(id.clone())
                        .debug_selector(move || id.clone())
                        .aria_label(format!("{} distance: {} pixels", d.edge, d.value))
                        .absolute()
                        .left(px(r.x))
                        .top(px(r.y))
                        .w(px(r.width))
                        .h(px(r.height))
                        .flex()
                        .items_center()
                        .justify_center()
                        .whitespace_nowrap()
                        .text_size(px(10.))
                        .line_height(px(18.))
                        .rounded(px(4.))
                        .overflow_hidden()
                        .bg(Color::PrimarySurface.get())
                        .text_color(Color::Accent.get())
                        .when(d.compact, |badge| {
                            badge.px(px(6.)).justify_between().child(
                                gpui::div().text_color(Color::SecondaryText.get()).child(
                                    match d.edge {
                                        "left" => "L",
                                        "top" => "T",
                                        "right" => "R",
                                        _ => "B",
                                    },
                                ),
                            )
                        })
                        .child(format!("{} px", d.value))
                })
            }))
            .when_some(guides.dimensions, |d, r| {
                d.child(
                    gpui::div()
                        .absolute()
                        .left(px(r.x))
                        .top(px(r.y))
                        .w(px(r.width))
                        .h(px(r.height))
                        .flex()
                        .items_center()
                        .justify_center()
                        .whitespace_nowrap()
                        .rounded(px(4.))
                        .bg(Color::PrimarySurface.get())
                        .text_size(px(10.))
                        .line_height(px(18.))
                        .text_color(Color::SecondaryText.get())
                        .child(format!("{} × {}", p.width, p.height)),
                )
            })
            .children(guides.handles.into_iter().enumerate().map(|(index, r)| {
                let corner = index as u8 + 1;
                let id = if corner == 1 {
                    "camera-resize-handle".to_owned()
                } else {
                    format!("camera-resize-handle-{corner}")
                };
                let marker = 6_f32.min(r.width);
                // Hit targets and visible squares share the same actual corner.
                // Place them in canvas coordinates, independent of the selection border.
                let (x, y) = match corner {
                    1 => (p.x + p.width, p.y + p.height),
                    2 => (p.x, p.y + p.height),
                    3 => (p.x, p.y),
                    _ => (p.x + p.width, p.y),
                };
                let mx =
                    (picture.x + x as f32 * sx - marker / 2.).clamp(0., (width - marker).max(0.));
                let my =
                    (picture.y + y as f32 * sy - marker / 2.).clamp(0., (height - marker).max(0.));
                gpui::div()
                    .id(id.clone())
                    .debug_selector(move || id.clone())
                    .absolute()
                    .left(px(r.x))
                    .top(px(r.y))
                    .w(px(r.width))
                    .h(px(r.height))
                    .cursor(if matches!(corner, 1 | 3) {
                        gpui::CursorStyle::ResizeUpLeftDownRight
                    } else {
                        gpui::CursorStyle::ResizeUpRightDownLeft
                    })
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |home, event: &gpui::MouseDownEvent, _, cx| {
                            home.camera_ui.drag = Some((event.position, p, corner));
                            cx.stop_propagation();
                        }),
                    )
                    .child(
                        gpui::div()
                            .absolute()
                            .left(px(mx - r.x))
                            .top(px(my - r.y))
                            .size(px(marker))
                            .rounded(px(1.))
                            .border_1()
                            .border_color(Color::Accent.get())
                            .bg(Color::PrimaryText.get()),
                    )
            }))
            .child(
                gpui::div()
                    .absolute()
                    .left(px(guides.done.x))
                    .top(px(guides.done.y))
                    .w(px(guides.done.width))
                    .h(px(guides.done.height))
                    .child(
                        Button::new("camera-edit-done", "Done")
                            .size(ButtonSize::Small)
                            .fill_width()
                            .on_click(cx.listener(|home, _, _, cx| {
                                home.camera_ui.editing = false;

                                home.camera_ui.drag = None;
                                home.persist(cx);
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }
    pub(super) fn selected_camera(&self) -> Option<&CameraSelection> {
        self.model
            .request
            .as_ref()
            .and_then(|r| r.camera.as_ref())
            .or(self.model.preferences.disabled_camera.as_ref())
    }
    pub(super) fn current_camera_format(&self) -> Option<&CameraFormat> {
        if self.camera_ui.invalid_format {
            return None;
        }
        let selected = self.selected_camera()?;
        self.model
            .formats
            .iter()
            .find(|f| f.index == selected.format_index && usable(f))
    }
    pub(super) fn suspend_startup_camera(&mut self) {
        if let Some(camera) = self.model.request.as_mut().and_then(|r| r.camera.take()) {
            self.model.preferences.disabled_camera = Some(camera);
        }
    }
    pub(super) fn toggle_camera(&mut self, cx: &mut Context<Self>) {
        if !self.model.editable() {
            return;
        }
        if let Some(request) = &mut self.model.request {
            if let Some(camera) = request.camera.take() {
                self.model.preferences.disabled_camera = Some(camera);
            } else if let Some(camera) = self
                .model
                .preferences
                .disabled_camera
                .as_ref()
                .filter(|c| {
                    self.model.cameras.iter().any(|d| d.id == c.device_id)
                        && self
                            .model
                            .formats
                            .iter()
                            .any(|f| f.index == c.format_index && usable(f))
                })
                .cloned()
            {
                request.camera = Some(camera);
                self.model.preferences.disabled_camera = None;
            }
            self.camera_ui.editing = false;

            self.persist(cx);
        }
    }
    pub(super) fn load_cameras(&mut self, cx: &mut Context<Self>) {
        if self.camera_ui.loading || self.model.closing {
            return;
        }
        #[cfg(test)]
        {
            self.camera_ui.loaded = true;
            let _ = cx;
        }

        #[cfg(all(target_os = "windows", not(test)))]
        {
            self.camera_ui.loading = true;
            let task = cx.background_executor().spawn(async {
                (
                    frame_windows::camera::enumerate_cameras(),
                    frame_windows::windows_build_number().is_ok_and(|build| build >= 22000),
                )
            });
            cx.spawn(async move |view, cx| {
                let (result, settings_supported) = task.await;
                let _ = view.update(cx, |this, cx| {
                    this.camera_ui.loading = false;
                    this.camera_ui.loaded = result.is_ok();
                    this.camera_ui.settings_supported = settings_supported;
                    if this.model.closing {
                        return;
                    }
                    match result {
                        Ok(cameras) => {
                            this.model.cameras = cameras;
                            if let Some(id) = this
                                .model
                                .request
                                .as_ref()
                                .and_then(|r| r.camera.as_ref())
                                .filter(|camera| {
                                    this.model.cameras.iter().any(|c| c.id == camera.device_id)
                                })
                                .map(|c| c.device_id.clone())
                            {
                                this.query_camera(id, false, cx);
                            } else {
                                this.model.formats.clear();
                            }
                        }
                        Err(error) => this.error(error),
                    }
                    cx.notify();
                });
            })
            .detach();
            cx.notify();
        }
        #[cfg(all(not(target_os = "windows"), not(test)))]
        {
            self.camera_ui.loaded = true;
            let _ = cx;
        }
    }
    pub(super) fn select_camera(&mut self, id: String, cx: &mut Context<Self>) {
        if !self.model.editable() || !self.model.cameras.iter().any(|camera| camera.id == id) {
            return;
        }
        self.query_camera(id, true, cx);
    }
    #[cfg(target_os = "windows")]
    fn query_camera(&mut self, id: String, replace: bool, cx: &mut Context<Self>) {
        if !self.model.editable() {
            return;
        }
        self.model.command_pending = true;
        self.camera_ui.invalid_format = true;
        self.camera_ui.generation += 1;
        let generation = self.camera_ui.generation;
        let old = self.selected_camera().cloned();
        let signature = old
            .as_ref()
            .filter(|c| c.device_id == id)
            .and_then(|c| {
                self.model
                    .formats
                    .iter()
                    .find(|f| f.index == c.format_index)
                    .or(self.model.preferences.camera_format.as_ref())
            })
            .cloned();
        self.model.formats.clear();
        let task = cx.background_executor().spawn(async move {
            frame_windows::camera::enumerate_camera_formats(&id).map(|formats| (id, formats))
        });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                if generation != this.camera_ui.generation { return; }
                this.model.command_pending = false;
                if this.model.closing || this.model.snapshot.state != RecorderState::Idle {
                    return;
                }
                match result {
                    Ok((id, formats)) => {
                        this.model.formats = formats;
                        {
                            let format = refreshed_format(&this.model.formats,old.as_ref().filter(|c| c.device_id==id).map(|c|c.format_index),signature.as_ref(),replace);
                            if let Some(format) = format {
                                this.camera_ui.invalid_format=false;
                                this.model.preferences.camera_format=Some(format.clone());
                                let (w, h) = this.model.source_dimensions().unwrap_or((320, 180));
                                let placement =
                                    old.map(|old| old.placement).unwrap_or(CameraPlacement {
                                        x: 16.min(w.saturating_sub(320)),
                                        y: 16.min(h.saturating_sub(180)),
                                        width: 320.min(w),
                                        height: 180.min(h),
                                    });
                                let camera = CameraSelection {
                                        device_id: id,
                                        format_index: format.index,
                                        placement,
                                    };
                                if !replace && this.model.preferences.disabled_camera.is_some() { this.model.preferences.disabled_camera = Some(camera); }
                                else if let Some(request) = &mut this.model.request { request.camera = Some(camera); this.model.preferences.disabled_camera=None; }
                                this.persist(cx);
                            } else {
                                // An index may now name a different native format. Keep the
                                // saved signature for the next refresh, but never reinterpret it.
                                if let Some(c)=this.model.request.as_mut().and_then(|r|r.camera.as_mut()).or(this.model.preferences.disabled_camera.as_mut()) { c.format_index=u32::MAX; }
                                this.model.error =
                                    Some(if this.model.formats.iter().any(usable) { "The saved camera capability is no longer available. Choose a supported resolution." } else { "This camera has no supported recording formats." }.into());
                            }
                        }
                    }
                    Err(error) => {
                        this.camera_ui.invalid_format = false;
                        if let Some(camera) = this.model.request.as_mut().and_then(|r| r.camera.take()) {
                            this.model.preferences.disabled_camera = Some(camera);
                        }
                        this.error(error);
                    },
                }
                cx.notify();
            });
        })
        .detach();
    }
    #[cfg(not(target_os = "windows"))]
    fn query_camera(&mut self, _: String, _: bool, _: &mut Context<Self>) {}
    pub(super) fn select_camera_format(&mut self, index: u32, cx: &mut Context<Self>) {
        if !self.model.editable()
            || self
                .model
                .request
                .as_ref()
                .and_then(|r| r.camera.as_ref())
                .or(self.model.preferences.disabled_camera.as_ref())
                .is_none_or(|camera| {
                    !self
                        .model
                        .cameras
                        .iter()
                        .any(|device| device.id == camera.device_id)
                })
            || !self
                .model
                .formats
                .iter()
                .any(|format| format.index == index && usable(format))
        {
            return;
        }
        if let Some(camera) = self
            .model
            .request
            .as_mut()
            .and_then(|r| r.camera.as_mut())
            .or(self.model.preferences.disabled_camera.as_mut())
        {
            self.camera_ui.invalid_format = false;
            camera.format_index = index;
            self.model.preferences.camera_format = self
                .model
                .formats
                .iter()
                .find(|f| f.index == index)
                .cloned();
            self.persist(cx);
        }
    }
    pub(super) fn open_camera_settings(&self, cx: &mut Context<Self>) {
        if !self.model.editable() || !self.camera_ui.settings_supported {
            return;
        }
        #[cfg(test)]
        {
            let _ = cx;
        }

        #[cfg(not(test))]
        if let Some(camera) = self.selected_camera().filter(|camera| {
            self.model
                .cameras
                .iter()
                .any(|device| device.id == camera.device_id)
        }) {
            cx.open_url(&camera_settings_uri(&camera.device_id));
        }
    }
}

pub(super) fn moved_placement(
    p: CameraPlacement,
    dx: f32,
    dy: f32,
    resize: bool,
    source: (u32, u32),
) -> CameraPlacement {
    let (sw, sh) = source;
    if resize {
        let max_w = sw.saturating_sub(p.x).max(1);
        let max_h = sh.saturating_sub(p.y).max(1);
        let ratio = p.width.max(1) as f32 / p.height.max(1) as f32;
        let desired = (p.width as f32 + dx)
            .max((p.height as f32 + dy) * ratio)
            .max(1.);
        let w = desired
            .min(max_w as f32)
            .min(max_h as f32 * ratio)
            .round()
            .max(1.) as u32;
        CameraPlacement {
            width: w.min(max_w),
            height: (w as f32 / ratio).round().max(1.).min(max_h as f32) as u32,
            ..p
        }
    } else {
        CameraPlacement {
            x: (p.x as f32 + dx)
                .round()
                .clamp(0., sw.saturating_sub(p.width) as f32) as u32,
            y: (p.y as f32 + dy)
                .round()
                .clamp(0., sh.saturating_sub(p.height) as f32) as u32,
            ..p
        }
    }
}

pub(super) fn corner_placement(
    p: CameraPlacement,
    dx: f32,
    dy: f32,
    corner: u8,
    source: (u32, u32),
) -> CameraPlacement {
    let left = matches!(corner, 2 | 3);
    let top = matches!(corner, 3 | 4);
    let ax = if left { p.x + p.width } else { p.x };
    let ay = if top { p.y + p.height } else { p.y };
    let ratio = p.width.max(1) as f32 / p.height.max(1) as f32;
    let delta = if dx.abs() >= dy.abs() * ratio {
        if left { -dx } else { dx }
    } else {
        (if top { -dy } else { dy }) * ratio
    };
    let max_w = if left {
        ax
    } else {
        source.0.saturating_sub(ax)
    };
    let max_h = if top { ay } else { source.1.saturating_sub(ay) };
    let w = (p.width as f32 + delta)
        .max(1.)
        .min(max_w.max(1) as f32)
        .min(max_h.max(1) as f32 * ratio)
        .round()
        .max(1.) as u32;
    let h = (w as f32 / ratio).round().max(1.).min(max_h.max(1) as f32) as u32;
    CameraPlacement {
        x: if left { ax.saturating_sub(w) } else { ax },
        y: if top { ay.saturating_sub(h) } else { ay },
        width: w,
        height: h,
    }
}

pub(super) fn usable(f: &CameraFormat) -> bool {
    f.supported && f.width > 0 && f.height > 0 && f.frame_rate_num > 0 && f.frame_rate_den > 0
}
pub(super) fn same_rate(a: &CameraFormat, b: &CameraFormat) -> bool {
    u64::from(a.frame_rate_num) * u64::from(b.frame_rate_den)
        == u64::from(b.frame_rate_num) * u64::from(a.frame_rate_den)
}
pub(super) fn same_format(a: &CameraFormat, b: &CameraFormat) -> bool {
    a.width == b.width && a.height == b.height && same_rate(a, b) && a.subtype == b.subtype
}
pub(super) fn refreshed_format<'a>(
    formats: &'a [CameraFormat],
    old_index: Option<u32>,
    signature: Option<&CameraFormat>,
    replace: bool,
) -> Option<&'a CameraFormat> {
    signature
        .and_then(|s| formats.iter().find(|f| usable(f) && same_format(f, s)))
        .or_else(|| {
            if signature.is_none() {
                old_index.and_then(|i| formats.iter().find(|f| usable(f) && f.index == i))
            } else {
                None
            }
        })
        .or_else(|| {
            if replace {
                formats.iter().find(|f| usable(f))
            } else {
                None
            }
        })
}
/// Deduplicate one axis without inventing combinations; prefer the other selected dimensions.
pub(super) fn choices<'a>(
    formats: &'a [CameraFormat],
    current: Option<&CameraFormat>,
    axis: usize,
) -> Vec<&'a CameraFormat> {
    let mut sorted: Vec<_> = formats
        .iter()
        .filter(|f| usable(f))
        .filter(|f| {
            current.is_none_or(|c| {
                axis == 0
                    || (f.width == c.width
                        && f.height == c.height
                        && (axis == 1 || same_rate(f, c)))
            })
        })
        .collect();
    sorted.sort_by(|a, b| {
        b.width
            .cmp(&a.width)
            .then(b.height.cmp(&a.height))
            .then(
                (u64::from(b.frame_rate_num) * u64::from(a.frame_rate_den))
                    .cmp(&(u64::from(a.frame_rate_num) * u64::from(b.frame_rate_den))),
            )
            .then(a.subtype.cmp(&b.subtype))
    });
    if let Some(c) = current {
        sorted.sort_by_key(|f| (!same_rate(f, c), f.subtype != c.subtype));
    }
    let mut unique: Vec<&CameraFormat> = Vec::new();
    for f in sorted {
        if !unique.iter().any(|g| match axis {
            0 => f.width == g.width && f.height == g.height,
            1 => same_rate(f, g),
            _ => f.subtype == g.subtype,
        }) {
            unique.push(f);
        }
    }
    unique
}

#[cfg(test)]
#[test]
fn camera_settings_uri_escapes_the_device_symbolic_link() {
    assert_eq!(
        camera_settings_uri(r"\\?\USB#VID_1234&MI_00"),
        "ms-settings:camera?cameraId=%5C%5C%3F%5CUSB%23VID_1234%26MI_00"
    );
}

#[cfg(test)]
mod guide_tests {
    use super::*;

    #[test]
    fn camera_guides_keep_four_distances_clear_of_handles_and_done() {
        for source in [(1920, 1080), (1080, 1920), (7680, 1080)] {
            for viewport in [(200., 130.), (280., 150.), (640., 360.), (980., 700.)] {
                let picture = Rect {
                    x: 0.,
                    y: 0.,
                    width: viewport.0,
                    height: viewport.1,
                }
                .fit(source);
                for (x, y, width, height) in [
                    (0, 0, 320, 180),
                    (source.0 - 320, 0, 320, 180),
                    (0, source.1 - 180, 320, 180),
                    (source.0 - 320, source.1 - 180, 320, 180),
                    (0, 0, source.0, source.1),
                    (source.0 / 2, source.1 / 2, 1, 1),
                ] {
                    let layout = placement_guides(
                        CameraPlacement {
                            x,
                            y,
                            width,
                            height,
                        },
                        source,
                        picture,
                        viewport,
                    );
                    let mut occupied = layout.handles.to_vec();
                    assert!(!occupied.iter().any(|r| r.overlaps(layout.done)));
                    occupied.push(layout.done);
                    for distance in &layout.distances {
                        let r = distance
                            .label
                            .expect("all four output-pixel distances must remain readable");
                        assert!(
                            r.x >= 0.
                                && r.y >= 0.
                                && r.x + r.width <= viewport.0
                                && r.y + r.height <= viewport.1
                        );
                        assert!(!occupied.iter().any(|o| o.overlaps(r)), "{}", distance.edge);
                        occupied.push(r);
                    }
                    if let Some(r) = layout.dimensions {
                        assert!(!occupied.iter().any(|o| o.overlaps(r)));
                    }
                    if layout.distances.iter().any(|d| d.compact) {
                        let [left, top, right, bottom] =
                            layout.distances.each_ref().map(|d| d.label.unwrap());
                        assert!(layout.distances.iter().all(|d| d.compact));
                        assert_eq!(left.width, right.width);
                        assert_eq!(left.y, right.y);
                        assert_eq!(top.y, bottom.y);
                        assert_eq!(left.x, top.x);
                        assert_eq!(right.x, bottom.x);
                    }
                }
            }
        }
    }

    #[test]
    fn camera_measurement_rules_use_the_letterboxed_picture_origin_and_output_pixels() {
        let picture = Rect {
            x: 0.,
            y: 0.,
            width: 640.,
            height: 500.,
        }
        .fit((1920, 1080));
        let layout = placement_guides(
            CameraPlacement {
                x: 100,
                y: 200,
                width: 320,
                height: 180,
            },
            (1920, 1080),
            picture,
            (640., 500.),
        );
        assert_eq!(
            layout.distances.each_ref().map(|d| d.value),
            [100, 200, 1500, 700]
        );
        assert_eq!(layout.distances[0].start.0, picture.x);
        assert_eq!(layout.distances[1].start.1, picture.y);
        assert_eq!(layout.distances[2].end.0, picture.x + picture.width);
        assert_eq!(layout.distances[3].end.1, picture.y + picture.height);
    }
}
