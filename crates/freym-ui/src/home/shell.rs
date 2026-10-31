use super::*;
// GPUI 0.2.2's test platform leaves open_with_system unimplemented. Observe
// the destination in headless tests without launching Explorer.
#[cfg(test)]
thread_local! {
    pub(super) static OPENED_OUTPUT: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}
use gpui::{AnimationExt, SpringAnimation, SpringPlayback, div};

use crate::ui::{components::*, primitives::*, theme::colors::Color};

#[derive(Clone, Debug, PartialEq)]
enum ListItem {
    Audio(AudioSourceId),
    Display(CaptureTarget),
    Camera(String),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ListPreview {
    from: ListItem,
    to: ListItem,
}
impl ListPreview {
    fn presentation(&self, item: &ListItem, items: &[ListItem]) -> (f32, bool, bool) {
        let (Some(from), Some(to), Some(index)) = (
            items.iter().position(|i| i == &self.from),
            items.iter().position(|i| i == &self.to),
            items.iter().position(|i| i == item),
        ) else {
            return (0., false, false);
        };
        let shift = if from < to && index > from && index <= to {
            -38.
        } else if from > to && index >= to && index < from {
            38.
        } else {
            0.
        };
        (shift, item == &self.from, item == &self.to)
    }
}

fn list_row_visual(
    row: impl IntoElement,
    id: String,
    item: &ListItem,
    items: &[ListItem],
    preview: Option<&ListPreview>,
) -> AnyElement {
    let (offset, held, destination) =
        preview.map_or((0., false, false), |p| p.presentation(item, items));
    let animation = SpringAnimation::new(crate::ui::theme::motion::response_spring(160.))
        .to(offset)
        .with_epsilon(0.1)
        .playback(if preview.is_some() {
            SpringPlayback::Running
        } else {
            SpringPlayback::Completed
        });
    let marker = format!("{id}-destination");
    div()
        .relative()
        .w_full()
        .h(px(36.))
        .when(destination, |d| {
            d.child(
                div()
                    .id(marker.clone())
                    .debug_selector(move || marker)
                    .absolute()
                    .inset_0()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(Color::Accent.get().opacity(0.25))
                    .bg(Color::Accent.get().opacity(0.035))
                    .aria_label("Drop here to reorder"),
            )
        })
        .child(
            div()
                .id(id.clone())
                .debug_selector(move || id)
                .relative()
                .w_full()
                .h(px(36.))
                .opacity(if held { 0. } else { 1. })
                .child(row)
                .with_spring("list-reorder-position", animation, |d, offset| {
                    d.top(px(offset))
                }),
        )
        .into_any_element()
}

fn begin_list_drag(
    owner: gpui::WeakEntity<Home>,
    drag: &ListDrag,
    cx: &mut gpui::App,
) -> Entity<ListDrag> {
    let _ = owner.update(cx, |home, cx| {
        home.list_preview = Some(ListPreview {
            from: drag.item.clone(),
            to: drag.item.clone(),
        });
        cx.notify();
    });
    cx.new(|cx| {
        // Covers release outside the list, Escape and window teardown as well as a drop.
        cx.on_release(move |_, cx| {
            let _ = owner.update(cx, |home, cx| {
                if home.list_preview.take().is_some() {
                    cx.notify();
                }
            });
        })
        .detach();
        drag.clone()
    })
}

#[derive(Clone)]
pub(super) struct ListDrag {
    owner: gpui::EntityId,
    item: ListItem,
    label: gpui::SharedString,
    width: f32,
}
pub(super) fn can_place_source(value: &dyn std::any::Any, owner: gpui::EntityId) -> bool {
    value.downcast_ref::<ListDrag>().is_some_and(|drag| {
        drag.owner == owner && matches!(drag.item, ListItem::Display(_) | ListItem::Camera(_))
    })
}
impl Home {
    pub(super) fn place_source(
        &mut self,
        drag: &ListDrag,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if drag.owner != cx.entity_id() || !self.model.editable() {
            return;
        }
        if let ListItem::Camera(id) = &drag.item {
            if self.selected_camera().is_some_and(|c| &c.device_id == id) {
                self.camera_ui.drag = None;
                if self
                    .model
                    .request
                    .as_ref()
                    .is_some_and(|r| r.camera.is_none())
                {
                    self.toggle_camera(cx);
                }
                self.camera_ui.editing = self
                    .model
                    .request
                    .as_ref()
                    .is_some_and(|r| r.camera.is_some());
                self.model.sheet = None;
                self.list_preview = None;
                cx.notify();
            }
            return;
        }

        let ListItem::Display(target) = &drag.item else {
            return;
        };
        self.list_preview = None;
        match target {
            CaptureTarget::Display(id) => {
                self.model.mode = SourceMode::Screen;
                self.dispatch(Action::SelectDisplay(id.clone()), window, cx);
            }
            CaptureTarget::Window(id) => {
                self.choose_window(id.clone(), cx);
            }
            CaptureTarget::Region(region) => {
                if let Some(request) = &mut self.model.request {
                    request.target = CaptureTarget::Region(region.clone());
                }
                self.model.mode = SourceMode::Area;
                self.persist(cx);
            }
            CaptureTarget::Game(_) => return,
        }
        cx.notify();
    }
}
impl gpui::Render for ListDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let width = self.width;
        div()
            .id("list-drag-preview")
            .debug_selector(|| "list-drag-preview".into())
            .w(px(width))
            .h(px(36.))
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(8.))
            .overflow_hidden()
            .rounded(px(6.))
            .bg(Color::ElevatedSurface.get())
            .border_1()
            .border_color(Color::SubtleBorder.get())
            .text_color(Color::PrimaryText.get())
            .text_size(px(13.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(self.label.clone()),
            )
            .child(
                div()
                    .w(px(28.))
                    .flex_shrink_0()
                    .flex()
                    .justify_center()
                    .child(Icon::new(Icons::Grip).size(IconSize::Small)),
            )
    }
}
fn list_drag_handle(group: String, id: String, enabled: bool) -> AnyElement {
    let debug = id.clone();
    div()
        .id(id)
        .debug_selector(move || debug.clone())
        .w(px(28.))
        .h(px(36.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .opacity(0.)
        .group_hover(group.clone(), |s| {
            s.opacity(if enabled { 0.65 } else { 0.25 })
        })
        .group_active(group, |s| s.opacity(if enabled { 1. } else { 0.25 }))
        .aria_label("Hold and drag anywhere in the row to reorder")
        .child(Icon::new(Icons::Grip).size(IconSize::Small))
        .when(enabled, |d| {
            d.cursor(gpui::CursorStyle::OpenHand)
                .hover(|s| s.opacity(1.))
                .on_click(|_, _, cx| cx.stop_propagation())
        })
        .into_any_element()
}
fn move_row<T: PartialEq>(rows: &mut Vec<T>, from: &T, to: &T) -> bool {
    let (Some(from), Some(to)) = (
        rows.iter().position(|r| r == from),
        rows.iter().position(|r| r == to),
    ) else {
        return false;
    };
    if from == to {
        return false;
    }
    let row = rows.remove(from);
    rows.insert(to, row);
    true
}

impl Home {
    fn preview_list_row(
        &mut self,
        event: &gpui::DragMoveEvent<ListDrag>,
        target: &ListItem,
        cx: &mut Context<Self>,
    ) {
        if self.list_preview.is_none() || !event.bounds.contains(&event.event.position) {
            return;
        }
        let drag = event.drag(cx);
        if !self.model.editable()
            || drag.owner != cx.entity_id()
            || std::mem::discriminant(&drag.item) != std::mem::discriminant(target)
        {
            return;
        }
        let preview = ListPreview {
            from: drag.item.clone(),
            to: target.clone(),
        };
        if self.list_preview.as_ref() != Some(&preview) {
            self.list_preview = Some(preview);
            cx.notify();
        }
    }
    fn track_list_drag_region(
        &mut self,
        event: &gpui::DragMoveEvent<ListDrag>,
        heading_height: f32,
        cx: &mut Context<Self>,
    ) {
        // Capture runs before row listeners, including rows clipped by scrolling.
        // They may propose a slot only while the pointer is inside this list body.
        let inside = event.bounds.contains(&event.event.position)
            && event.event.position.y >= event.bounds.top() + px(heading_height);
        if !inside {
            if self.list_preview.take().is_some() {
                cx.notify();
            }
        } else if self.model.editable()
            && event.drag(cx).owner == cx.entity_id()
            && self.list_preview.is_none()
        {
            let item = event.drag(cx).item.clone();
            self.list_preview = Some(ListPreview {
                from: item.clone(),
                to: item,
            });
            cx.notify();
        }
    }
    fn reorder_row(&mut self, drag: &ListDrag, target: &ListItem, cx: &mut Context<Self>) {
        if !self.model.editable() || drag.owner != cx.entity_id() {
            return;
        }
        let preview = self.list_preview.take();
        let target = preview
            .as_ref()
            .filter(|p| p.from == drag.item)
            .map_or(target, |p| &p.to);
        let changed = match (&drag.item, target) {
            (ListItem::Audio(from), ListItem::Audio(to)) => {
                move_row(&mut self.audio_ui.order, from, to)
            }
            (ListItem::Display(from), ListItem::Display(to)) => {
                move_row(&mut self.source_shelf_targets, from, to)
            }
            _ => false,
        };
        if changed || preview.is_some() {
            cx.notify();
        }
    }
    pub(super) fn source_shelf(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let editable = self.model.editable();
        let selectable = self.panels_interactive();
        let mut sources = div()
            .id("source-shelf-list")
            .debug_selector(|| "source-shelf-list".into())
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(2.));
        // A shelf contains explicitly chosen targets, not every enumerated window.
        // Selection stays on the existing single-target recording request.
        if let Some(target) = self.model.request.as_ref().map(|r| r.target.clone())
            && !self.source_shelf_targets.contains(&target)
        {
            self.source_shelf_targets.push(target);
        }
        let items = self
            .source_shelf_targets
            .iter()
            .cloned()
            .map(ListItem::Display)
            .collect::<Vec<_>>();
        for (index, target) in self.source_shelf_targets.iter().enumerate() {
            let selected = self
                .model
                .request
                .as_ref()
                .is_some_and(|r| &r.target == target);
            let (label, description, kind, available, _action) = match target {
                CaptureTarget::Display(id) => {
                    let display = self.model.snapshot.displays.iter().find(|d| &d.id == id);
                    (
                        display
                            .map(|d| d.name.clone())
                            .unwrap_or_else(|| "Unavailable display".into()),
                        display.map(|d| {
                            format!(
                                "{} × {}{}",
                                d.width,
                                d.height,
                                d.refresh_rate_hz
                                    .filter(|v| v.is_finite() && *v > 0.)
                                    .map(|v| format!(" · {v:.0} Hz"))
                                    .unwrap_or_default()
                            )
                        }),
                        CaptureSourceKind::Display,
                        display.is_some(),
                        Action::SelectDisplay(id.clone()),
                    )
                }
                CaptureTarget::Window(id) => {
                    let source = self.model.snapshot.windows.iter().find(|w| &w.id == id);
                    (
                        source
                            .map(|w| w.title.clone())
                            .unwrap_or_else(|| "Unavailable window".into()),
                        source.and_then(|w| w.process_name.clone()),
                        CaptureSourceKind::Window,
                        source.is_some(),
                        Action::SelectWindow(id.clone()),
                    )
                }
                // An existing area remains available through its exact region. The
                // picker still owns creating or changing area bounds.
                CaptureTarget::Region(region) => (
                    "Selected area".into(),
                    Some(format!("{} × {}", region.width, region.height)),
                    CaptureSourceKind::Region,
                    true,
                    Action::OpenSourcePicker,
                ),
                CaptureTarget::Game(_) => continue,
            };
            let mut card = CaptureSourceCard::new(format!("shelf-source-{index}"), label.clone())
                .list_row()
                .kind(kind)
                .selected(selected)
                .disabled(!selectable || !available);
            if self.model.staged_target.as_ref() == Some(target) {
                card = card.description("Next recording");
            } else if let Some(description) = description {
                card = card.description(description);
            }
            let destination = ListItem::Display(target.clone());
            let hover_target = destination.clone();
            let owner = cx.entity_id();
            let drag_home = cx.weak_entity();
            let drag = ListDrag {
                owner,
                item: destination.clone(),
                label: label.into(),
                width: (self.workspace_width(crate::state::workspace::Panel::Display) - 12.)
                    .max(1.),
            };
            card = card.drag_handle(list_drag_handle(
                format!("display-source-entry-shelf-source-{index}"),
                format!("display-drag-{index}"),
                editable,
            ));
            let preview_target = target.clone();
            #[cfg(target_os = "windows")]
            let preview_sink = self
                .model
                .request
                .as_ref()
                .filter(|request| &request.target == target)
                .and_then(|_| self.services.control.preview_hub())
                .and_then(|hub| hub.gpu_sink());
            let target = target.clone();
            let row = div()
                .id(format!("display-drop-{index}"))
                .debug_selector(move || format!("display-drop-{index}"))
                .w_full()
                .h(px(36.))
                .rounded(px(6.))
                .when(
                    available && matches!(target, CaptureTarget::Display(_)),
                    |d| {
                        d.tooltip(move |_, cx| {
                            let target = preview_target.clone();
                            #[cfg(target_os = "windows")]
                            let sink = preview_sink.clone();
                            let fetch: crate::ui::components::source_thumbnail::ThumbnailRequest =
                                Box::new(move |cancel| {
                                    #[cfg(target_os = "windows")]
                                    {
                                        frame_windows::preview::source_thumbnail(
                                            &target,
                                            cancel,
                                            sink.as_deref(),
                                        )
                                    }
                                    #[cfg(not(target_os = "windows"))]
                                    {
                                        let _ = (target, cancel);
                                        Err("Source previews are unavailable on this platform"
                                            .into())
                                    }
                                });
                            cx.new(|cx| {
                                crate::ui::components::source_thumbnail::SourceThumbnail::new(
                                    fetch, cx,
                                )
                            })
                            .into()
                        })
                    },
                )
                .when(editable, |d| {
                    d.cursor(gpui::CursorStyle::OpenHand)
                        .on_drag(drag, move |drag, _, _, cx| {
                            begin_list_drag(drag_home.clone(), drag, cx)
                        })
                })
                .on_drag_move(cx.listener(move |home, event, _, cx| {
                    home.preview_list_row(event, &hover_target, cx)
                }))
                .can_drop(move |value, _, _| {
                    editable
                        && value.downcast_ref::<ListDrag>().is_some_and(|d| {
                            d.owner == owner && matches!(d.item, ListItem::Display(_))
                        })
                })
                .child(list_row_visual(
                    card.on_click(cx.listener(move |home, _, window, cx| {
                        if cx.has_active_drag() {
                            return;
                        }
                        home.dispatch(Action::SelectTarget(target.clone()), window, cx);
                    })),
                    format!("display-reorder-{index}"),
                    &destination,
                    &items,
                    self.list_preview.as_ref(),
                ))
                .on_drop(cx.listener(move |home, drag: &ListDrag, _, cx| {
                    home.reorder_row(drag, &destination, cx);
                }));
            sources = sources.child(row);
        }
        if let Some(camera) = self.selected_camera() {
            let drag = ListDrag {
                owner: cx.entity_id(),
                item: ListItem::Camera(camera.device_id.clone()),
                label: "Camera overlay".into(),
                width: 220.,
            };
            let drag_home = cx.weak_entity();
            let available = self.model.cameras.iter().any(|c| c.id == camera.device_id)
                && self.current_camera_format().is_some();
            let visible = self
                .model
                .request
                .as_ref()
                .is_some_and(|r| r.camera.is_some());
            let name = self
                .model
                .cameras
                .iter()
                .find(|c| c.id == camera.device_id)
                .map(|c| c.name.clone())
                .unwrap_or_else(|| "Camera overlay".into());
            sources =
                sources.child(
                    div()
                        .id("source-camera-row")
                        .debug_selector(|| "source-camera-row".into())
                        .w_full()
                        .flex()
                        .items_center()
                        .gap(px(2.))
                        .on_mouse_down(
                            gpui::MouseButton::Right,
                            cx.listener(|home, event, window, cx| {
                                home.open_source_menu(
                                    source_menu::SourceRow::Camera,
                                    event,
                                    window,
                                    cx,
                                );
                            }),
                        )
                        .child(
                            div().flex_1().min_w_0().child(
                                CaptureSourceCard::new("shelf-camera", name)
                                    .list_row()
                                    .kind(CaptureSourceKind::Camera)
                                    .selected(visible)
                                    .description(if visible {
                                        "Camera overlay"
                                    } else {
                                        "Camera overlay · hidden"
                                    })
                                    .disabled(!editable)
                                    .on_click(self.handler(Action::CameraPlacement, cx)),
                            ),
                        )
                        .when(editable, |d| {
                            d.on_drag(drag, move |drag, _, _, cx| {
                                begin_list_drag(drag_home.clone(), drag, cx)
                            })
                        })
                        .child(
                            Button::new(
                                "source-camera-visibility",
                                if visible {
                                    "Hide camera overlay"
                                } else {
                                    "Show camera overlay"
                                },
                            )
                            .size(ButtonSize::ListIcon)
                            .icon(if visible {
                                Icons::Visible
                            } else {
                                Icons::Hidden
                            })
                            .disabled(!editable || (!visible && !available))
                            .on_click(self.handler(
                                Action::Video(crate::video::VideoAction::CameraToggle),
                                cx,
                            )),
                        )
                        .child(
                            Button::new("source-camera-edit", "Edit camera position in Preview")
                                .size(ButtonSize::ListIcon)
                                .icon(Icons::Area)
                                .disabled(!editable || !visible)
                                .on_click(self.handler(Action::CameraPlacement, cx)),
                        )
                        .child(
                            Button::new("source-camera-remove", "Remove camera overlay")
                                .size(ButtonSize::ListIcon)
                                .icon(Icons::Close)
                                .disabled(!editable)
                                .on_click(self.handler(Action::RemoveCamera, cx)),
                        ),
                );
        }
        let element = div()
            .id("source-shelf")
            .debug_selector(|| "source-shelf".into())
            .size_full()
            .on_drag_move::<ListDrag>(cx.listener(
                |home, event: &gpui::DragMoveEvent<ListDrag>, _, cx| {
                    if matches!(event.drag(cx).item, ListItem::Display(_)) {
                        home.track_list_drag_region(event, 6., cx);
                    }
                },
            ))
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(2.))
            .px(px(6.))
            .child(sources);
        crate::ui::finish_design(
            crate::ui::designable(element, "source-shelf", "Sources list", cx),
            "source-shelf",
            cx,
        )
    }

    pub(super) fn handler(
        &self,
        action: Action,
        cx: &Context<Self>,
    ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static {
        let view = cx.weak_entity();
        let owner = self.owner_window;
        move |_, window, cx| {
            // Recording tasks always belong to the Recorder, even when the
            // originating compact menu is closed before acknowledgement arrives.
            if matches!(action, Action::Record | Action::Pause) && window.window_handle() != owner {
                let _ = owner.update(cx, |_, window, cx| {
                    let _ = view.update(cx, |this, cx| this.dispatch(action.clone(), window, cx));
                });
            } else {
                let _ = view.update(cx, |this, cx| this.dispatch(action.clone(), window, cx));
            }
        }
    }

    pub(super) fn button(
        &self,
        action: Action,
        label: impl Into<String>,
        glyph: Option<Icons>,
        selected: bool,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut button = Button::new(format!("{action:?}"), label.into())
            .selected(selected)
            .disabled(!enabled)
            .on_click(self.handler(action, cx));

        if let Some(glyph) = glyph {
            button = button.icon(glyph);
        }

        button.into_any_element()
    }

    pub(super) fn recording_button(&self, cx: &mut Context<Self>) -> RecordButton {
        let state = match self.model.snapshot.state {
            RecorderState::Idle => RecordButtonState::Ready,

            RecorderState::Preparing => RecordButtonState::Countdown,

            RecorderState::Recording => RecordButtonState::Recording,

            RecorderState::Paused => RecordButtonState::Paused,

            RecorderState::Finalizing => RecordButtonState::Stopping,

            RecorderState::Failed(_) => RecordButtonState::Failed,
        };

        let enabled = if state == RecordButtonState::Ready {
            self.model.can_start()
        } else {
            !self.model.command_pending
                && !self.model.closing
                && state != RecordButtonState::Stopping
        };

        let button = RecordButton::new(state)
            .label(self.model.record_label())
            .disabled(!enabled)
            .on_click(self.handler(Action::Record, cx));

        button
    }

    pub(super) fn home_audio_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let editable = self.model.editable();
        self.remember_recorder_audio_devices();
        let mut ordered_sources = self.recorder_audio_sources();
        self.audio_ui
            .ribbons
            .retain(|id, _| ordered_sources.iter().any(|s| &s.id == id));
        let mut panel = RecorderAudioPanel::new();
        let row_width = self.workspace_width(crate::state::workspace::Panel::Audio);
        self.audio_ui
            .order
            .retain(|id| ordered_sources.iter().any(|s| &s.id == id));
        for source in &ordered_sources {
            if !self.audio_ui.order.contains(&source.id) {
                self.audio_ui.order.push(source.id.clone());
            }
        }
        ordered_sources.sort_by_key(|s| self.audio_ui.order.iter().position(|id| id == &s.id));
        let items = ordered_sources
            .iter()
            .map(|s| ListItem::Audio(s.id.clone()))
            .collect::<Vec<_>>();
        if ordered_sources.is_empty() {
            panel = panel.child(Text::new("No audio sources").variant(TextVariant::Muted));
        }
        let first_mic = ordered_sources
            .iter()
            .find(|s| s.kind == AudioSourceKind::Microphone)
            .map(|s| &s.id);
        for source in &ordered_sources {
            let status = self.ribbon_status(source);
            let ribbon = self
                .audio_ui
                .ribbons
                .entry(source.id.clone())
                .or_insert_with(|| cx.new(|_| PulseRibbon::new(source.id.clone(), source.kind)))
                .clone();
            ribbon.update(cx, |ribbon, cx| {
                ribbon.set_available_width(row_width, cx);
                ribbon.set_status(status, cx);
            });
            let kind = match source.kind {
                AudioSourceKind::Microphone => AudioKind::Microphone,
                AudioSourceKind::System => AudioKind::System,
                AudioSourceKind::Application => AudioKind::Application,
            };
            let id = if first_mic == Some(&source.id) {
                "mic-toggle".into()
            } else {
                format!("home-audio-{}", source.id.0)
            };
            let routes = [
                AudioRoute::MainMix,
                AudioRoute::SeparateOnly,
                AudioRoute::MainMixAndSeparate,
            ];
            let selected = routes.iter().position(|r| *r == source.route).unwrap_or(0);
            let source_id = source.id.clone();
            let view = cx.weak_entity();
            let destination = ListItem::Audio(source.id.clone());
            let hover_target = destination.clone();
            let owner = cx.entity_id();
            let drag_home = cx.weak_entity();
            let drag = ListDrag {
                owner,
                item: destination.clone(),
                label: source.display_name.clone().into(),
                width: (self.workspace_width(crate::state::workspace::Panel::Audio) - 12.).max(1.),
            };
            let live = matches!(
                self.model.snapshot.state,
                RecorderState::Recording | RecorderState::Paused
            );
            let toggle = self.handler(
                if live {
                    Action::MuteAudio(source.id.clone(), !source.muted)
                } else {
                    Action::ToggleAudio(source.id.clone())
                },
                cx,
            );
            let menu_id = source.id.clone();
            let handle = list_drag_handle(
                format!("audio-source-entry-{id}"),
                format!("audio-drag-{}", source.id.0),
                editable,
            );
            panel = panel.child(
                div()
                    .id(format!("audio-drop-{}", source.id.0))
                    .debug_selector({
                        let id = source.id.0.clone();
                        move || format!("audio-drop-{id}")
                    })
                    .w_full()
                    .h(px(36.))
                    .rounded(px(6.))
                    .on_mouse_down(
                        gpui::MouseButton::Right,
                        cx.listener(move |home, event, window, cx| {
                            home.open_source_menu(
                                source_menu::SourceRow::Audio(menu_id.clone()),
                                event,
                                window,
                                cx,
                            );
                        }),
                    )
                    .when(editable, |d| {
                        d.cursor(gpui::CursorStyle::OpenHand)
                            .on_drag(drag, move |drag, _, _, cx| {
                                begin_list_drag(drag_home.clone(), drag, cx)
                            })
                    })
                    .on_drag_move(cx.listener(move |home, event, _, cx| {
                        home.preview_list_row(event, &hover_target, cx)
                    }))
                    .can_drop(move |value, _, _| {
                        editable
                            && value.downcast_ref::<ListDrag>().is_some_and(|d| {
                                d.owner == owner && matches!(d.item, ListItem::Audio(_))
                            })
                    })
                    .child(list_row_visual(
                        AudioSourceRow::new(id, source.display_name.clone(), kind)
                            .available_width(row_width)
                            .enabled(source.route != AudioRoute::Ignore)
                            .connected(source.is_active)
                            .disabled(if live {
                                !self.can_mute(&source.id)
                            } else {
                                !editable
                            })
                            .live_mute(live.then_some(source.muted))
                            .on_click(move |event, window, cx| {
                                if !cx.has_active_drag() {
                                    toggle(event, window, cx);
                                }
                            })
                            .inline()
                            .meter(ribbon)
                            .drag_handle(handle)
                            .controls(
                                div()
                                    .flex()
                                    .flex_shrink_0()
                                    .items_center()
                                    .gap(px(4.))
                                    .child(
                                        div().w(px(68.)).flex_shrink_0().child(
                                            CycleButton::new(
                                                format!("audio-routing-{}", source.id.0),
                                                "Audio routing",
                                                selected,
                                                ["Mixed", "Separate", "Both"]
                                                    .into_iter()
                                                    .map(|label| CycleOption {
                                                        label: label.into(),
                                                        enabled: true,
                                                    })
                                                    .collect(),
                                                move |index, window, cx| {
                                                    if cx.has_active_drag() {
                                                        return;
                                                    }
                                                    let _ = view.update(cx, |home, cx| {
                                                        home.dispatch(
                                                            Action::SelectRoute(
                                                                source_id.clone(),
                                                                routes[index],
                                                            ),
                                                            window,
                                                            cx,
                                                        )
                                                    });
                                                },
                                            )
                                            .rolling_text()
                                            .text_only(
                                                match (
                                                    if source.route == AudioRoute::Ignore {
                                                        usize::MAX
                                                    } else {
                                                        selected
                                                    },
                                                    crate::ui::theme::ThemeId::current()
                                                        == crate::ui::theme::ThemeId::White,
                                                ) {
                                                    (0, true) => gpui::rgb(0x2b66ad).into(),
                                                    (1, true) => gpui::rgb(0x7e4eb8).into(),
                                                    (2, true) => gpui::rgb(0x866018).into(),
                                                    (0, false) => gpui::rgb(0x8dbbff).into(),
                                                    (1, false) => gpui::rgb(0xc0a0ff).into(),
                                                    (2, false) => gpui::rgb(0xedc071).into(),
                                                    _ => Color::SecondaryText.get(),
                                                },
                                            )
                                            .disabled(
                                                !editable || source.route == AudioRoute::Ignore,
                                            ),
                                        ),
                                    ),
                            ),
                        format!("audio-reorder-{}", source.id.0),
                        &destination,
                        &items,
                        self.list_preview.as_ref(),
                    ))
                    .on_drop(cx.listener(move |home, drag: &ListDrag, _, cx| {
                        home.reorder_row(drag, &destination, cx);
                    })),
            );
        }
        div()
            .id("audio-list-drag-region")
            .size_full()
            .min_w_0()
            .on_drag_move::<ListDrag>(cx.listener(
                |home, event: &gpui::DragMoveEvent<ListDrag>, _, cx| {
                    if matches!(event.drag(cx).item, ListItem::Audio(_)) {
                        home.track_list_drag_region(event, 6., cx);
                    }
                },
            ))
            .child(panel)
            .into_any_element()
    }

    pub(super) fn setting(&self, label: &'static str, control: AnyElement) -> AnyElement {
        SettingRow::new(label, control).into_any_element()
    }

    pub(super) fn output_row(&self, cx: &mut Context<Self>) -> OutputFolderRow {
        let label = self
            .model
            .request
            .as_ref()
            .and_then(|r| std::path::Path::new(&r.output.directory).file_name())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Output".into());

        OutputFolderRow::new("Output", label)
            .disabled(!self.model.editable())
            .on_click(self.handler(Action::Output, cx))
    }

    pub(super) fn output_directory(&self) -> Option<&str> {
        self.model
            .request
            .as_ref()
            .map(|r| r.output.directory.as_str())
            .filter(|path| !path.is_empty())
    }

    pub(super) fn output_destination(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let directory = self
            .output_directory()
            .unwrap_or("Choose recording folder")
            .to_owned();
        let enabled = self.output_directory().is_some() && !self.model.closing;
        let focus = window.use_keyed_state("output-directory-focus", cx, |_, cx| {
            cx.focus_handle().tab_stop(true)
        });
        let focus = focus.read(cx).clone();
        let key_focus = focus.clone();
        let view = cx.weak_entity();
        div()
            .id("command-deck-output")
            .debug_selector(|| "command-deck-output".into())
            .w_full()
            .min_w_0()
            .h(px(28.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(div().w(px(28.)).flex_shrink_0())
            .child(
                div()
                    .id("open-output-directory")
                    .debug_selector(|| "open-output-directory".into())
                    .role(gpui::Role::Button)
                    .aria_label(format!("Open recording folder: {directory}"))
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(14.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(Color::PrimaryText.get())
                    .when(enabled, |d| {
                        d.track_focus(&focus)
                            .tab_index(0)
                            .cursor_pointer()
                            .hover(|d| d.text_color(Color::PrimaryText.get()).underline())
                            .focus_visible(|d| d.text_color(Color::PrimaryText.get()).underline())
                            .on_click(cx.listener(move |home, _, window, cx| {
                                window.focus(&focus, cx);
                                home.dispatch(Action::OpenOutput, window, cx);
                            }))
                            .on_key_down(move |event, window, cx| {
                                if key_focus.is_focused(window)
                                    && !event.is_held
                                    && matches!(event.keystroke.key.as_str(), "enter" | "space")
                                {
                                    cx.stop_propagation();
                                    let _ = view.update(cx, |home, cx| {
                                        home.dispatch(Action::OpenOutput, window, cx)
                                    });
                                }
                            })
                    })
                    .child(
                        div()
                            .debug_selector(|| "output-directory-text".into())
                            .min_w_0()
                            .truncate()
                            .child(directory),
                    ),
            )
            .child(
                Button::new("Output", "Choose recording folder")
                    .size(ButtonSize::ListIcon)
                    .variant(ButtonVariant::Ghost)
                    .icon(Icons::Folder)
                    .disabled(!self.model.editable())
                    .on_click(self.handler(Action::Output, cx)),
            )
            .into_any_element()
    }

    pub(super) fn toggle(
        &self,
        action: Action,
        label: &'static str,
        checked: bool,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        Toggle::new(format!("{action:?}"), label, checked)
            .disabled(!enabled)
            .on_click(self.handler(action, cx))
            .into_any_element()
    }

    pub(super) fn controls(&self, cx: &mut Context<Self>) -> RecordControls {
        let view = cx.weak_entity();
        let mut controls = RecordControls::new(self.recording_button(cx)).accessories([
            CountdownPicker::new(
                "home-countdown",
                self.model.preferences.countdown,
                move |value, window, cx| {
                    let _ = view.update(cx, |home, cx| {
                        home.dispatch(Action::SetCountdown(value), window, cx)
                    });
                },
            )
            .disabled(!self.model.editable())
            .into_any_element(),
            Button::new("home-settings", "Settings")
                .size(ButtonSize::Icon)
                .deck_control()
                .tooltip("Settings")
                .icon(Icons::Settings)
                .on_click(self.handler(Action::Settings, cx))
                .into_any_element(),
            Button::new("workspace-reset", "Reset panel layout")
                .size(ButtonSize::Icon)
                .deck_control()
                .tooltip("Reset panel layout")
                .icon(Icons::Refresh)
                .disabled(!self.preferences_loaded || self.model.closing)
                .on_click(cx.listener(|home, _, _, cx| {
                    if !cx.has_active_drag() {
                        home.reset_workspace(cx);
                    }
                }))
                .into_any_element(),
        ]);
        let split = !self.model.snapshot.replay_only
            && matches!(
                self.model.snapshot.state,
                RecorderState::Recording | RecorderState::Paused | RecorderState::Finalizing
            );
        let paused = self.model.snapshot.state == RecorderState::Paused;
        let elapsed = format_elapsed(
            self.model
                .snapshot
                .metrics
                .as_ref()
                .map(|m| m.elapsed_ms)
                .unwrap_or(0.),
        );
        let pause = Button::new(
            "Pause",
            if paused {
                "Resume recording"
            } else {
                "Pause recording"
            },
        )
        .size(ButtonSize::Record)
        .variant(ButtonVariant::Secondary)
        .disabled(
            !split
                || self.model.snapshot.state == RecorderState::Finalizing
                || !self.model.snapshot.capabilities.can_pause
                || self.model.command_pending,
        )
        .on_click(self.handler(Action::Pause, cx));

        controls = controls.session(pause, elapsed).split(split);
        controls
    }
}

impl Home {
    pub(super) fn panels_interactive(&self) -> bool {
        !self.model.connecting
            && !self.model.closing
            && !self.model.recovery_busy
            && matches!(
                self.model.snapshot.state,
                RecorderState::Idle | RecorderState::Recording | RecorderState::Paused
            )
    }
    pub(super) fn select_capture_target(
        &mut self,
        target: CaptureTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.panels_interactive() {
            return;
        }
        let available = match &target {
            CaptureTarget::Display(id) => self.model.snapshot.displays.iter().any(|d| &d.id == id),
            CaptureTarget::Window(id) => self.model.snapshot.windows.iter().any(|w| &w.id == id),
            CaptureTarget::Region(region) => self
                .model
                .snapshot
                .displays
                .iter()
                .any(|d| frame_settings::validate_region(region, d).is_ok()),
            CaptureTarget::Game(_) => false,
        };
        if !available {
            self.model.error =
                Some("This source is unavailable. Refresh Sources and choose again.".into());
            cx.notify();
            return;
        }
        if matches!(
            self.model.snapshot.state,
            RecorderState::Recording | RecorderState::Paused
        ) {
            self.model.staged_target = if self
                .model
                .request
                .as_ref()
                .is_some_and(|r| r.target == target)
            {
                None
            } else {
                Some(target.clone())
            };
            if !self.source_shelf_targets.contains(&target) {
                self.source_shelf_targets.push(target);
            }
            self.model.sheet = None;
            window.focus(&self.focus, cx);
            cx.notify();
            return;
        }
        if !self.model.editable() {
            return;
        }
        self.model.staged_target = None;
        match target {
            CaptureTarget::Display(id) => {
                self.model.mode = SourceMode::Screen;
                self.dispatch(Action::SelectDisplay(id), window, cx);
            }
            CaptureTarget::Window(id) => self.dispatch(Action::SelectWindow(id), window, cx),
            CaptureTarget::Region(region) => {
                if let Some(request) = &mut self.model.request {
                    request.target = CaptureTarget::Region(region);
                    request.resolution = Resolution::Native;
                }
                self.model.mode = SourceMode::Area;
                self.persist(cx);
            }
            CaptureTarget::Game(_) => (),
        }
    }
    pub(super) fn apply_staged_target(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.editable()
            && let Some(target) = self.model.staged_target.take()
        {
            self.select_capture_target(target.clone(), window, cx);
            if self.model.error.is_some()
                && self
                    .model
                    .request
                    .as_ref()
                    .is_none_or(|r| r.target != target)
            {
                self.model.staged_target = Some(target);
            }
        }
    }
}
