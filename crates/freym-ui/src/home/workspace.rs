//! Native panel docking for the recorder workspace.
use super::*;
use crate::{
    state::workspace::{Axis, DockLayout, Edge, Geometry, Panel, Rect, Split},
    ui::{components::*, primitives::*, theme::colors::Color},
};
use gpui::{CursorStyle, DragMoveEvent, KeyDownEvent, ParentElement, Render, div};

#[derive(Default)]
pub(super) struct WorkspaceUi {
    pub(super) resources: Option<Entity<crate::ui::components::resource_usage::ResourceUsage>>,
    memory_graph: Option<Entity<crate::ui::components::resource_usage::MemoryGraph>>,
    pub geometry: Geometry,
    pub dragging: Option<Panel>,
    target: Option<(Panel, Edge)>,
    pub(super) resize: Option<ResizeSession>,
    pub padding: f32,
    margin: f32,
    gap: f32,
    header: f32,
    viewport_height: f32,
    menu: Option<PanelMenu>,
    pub(super) source_menu: Option<source_menu::SourceMenu>,
    anchors: std::collections::BTreeMap<Panel, gpui::Bounds<gpui::Pixels>>,
}
pub(super) struct ResizeSession {
    initial: DockLayout,
    layout: DockLayout,
    grab_offset: f32,
}
impl WorkspaceUi {
    pub(super) fn set_resources_active(&self, active: bool, cx: &mut gpui::App) {
        if let Some(resources) = &self.resources {
            resources.update(cx, |view, _| view.set_active(active));
        }
    }
}
struct PanelMenu {
    panel: Panel,
    active: usize,
    focus: gpui::FocusHandle,
    previous: Option<gpui::FocusHandle>,
    audio_sources: Vec<AudioSource>,
    scroll: gpui::ScrollHandle,
}

#[derive(Clone)]
struct PanelDrag {
    owner: gpui::EntityId,
    panel: Panel,
}
impl Render for PanelDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .p(px(10.))
            .rounded(px(6.))
            .bg(Color::ElevatedSurface.base())
            .border_1()
            .border_color(gpui::rgb(0xa78bfa))
            .text_size(px(13.))
            .text_color(Color::PrimaryText.get())
            .child(format!("{} · dock", self.panel.title()))
    }
}
#[derive(Clone)]
struct SplitDrag {
    owner: gpui::EntityId,
    split: Split,
}
impl Render for SplitDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

fn box_at(rect: Rect) -> gpui::Div {
    div()
        .absolute()
        .left(px(rect.x))
        .top(px(rect.y))
        .w(px(rect.width))
        .h(px(rect.height))
}
fn target_edge(rect: Rect, x: f32, y: f32) -> Edge {
    let (cx, cy) = (rect.x + rect.width / 2., rect.y + rect.height / 2.);
    for (edge, dx, dy) in [
        (Edge::Left, -32., 0.),
        (Edge::Right, 32., 0.),
        (Edge::Top, 0., -32.),
        (Edge::Bottom, 0., 32.),
        (Edge::Swap, 0., 0.),
    ] {
        if (x - cx - dx).abs() <= 12. && (y - cy - dy).abs() <= 12. {
            return edge;
        }
    }
    let (u, v) = ((x - rect.x) / rect.width, (y - rect.y) / rect.height);
    let nearest = [
        (u, Edge::Left),
        (1. - u, Edge::Right),
        (v, Edge::Top),
        (1. - v, Edge::Bottom),
    ]
    .into_iter()
    .min_by(|a, b| a.0.total_cmp(&b.0))
    .unwrap();
    if nearest.0 < 0.24 {
        nearest.1
    } else {
        Edge::Swap
    }
}
impl Home {
    pub(super) fn panel_menu_open(&self) -> bool {
        self.workspace_ui.menu.is_some()
    }
    pub(super) fn close_panel_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(menu) = self.workspace_ui.menu.take() else {
            return false;
        };
        if let Some(previous) = menu.previous {
            window.focus(&previous, cx);
        }
        cx.notify();
        true
    }
    fn open_panel_menu(&mut self, panel: Panel, window: &mut Window, cx: &mut Context<Self>) {
        self.close_source_menu(window, cx);
        if self.close_panel_menu(window, cx) || !self.panels_interactive() {
            return;
        }
        let focus = cx.focus_handle().tab_stop(false);
        self.workspace_ui.menu = Some(PanelMenu {
            panel,
            active: 0,
            previous: window.focused(cx),
            focus: focus.clone(),
            audio_sources: if panel == Panel::Audio {
                self.additional_audio_sources()
            } else {
                vec![]
            },
            scroll: gpui::ScrollHandle::new(),
        });
        window.focus(&focus, cx);
        cx.notify();
    }
    fn select_panel_menu(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.workspace_ui.menu.as_ref().map(|m| m.panel) else {
            return;
        };
        // Keep the ID captured when the menu opened, even if discovery reorders.
        let source_id = self.workspace_ui.menu.as_ref().and_then(|m| {
            index
                .checked_sub(2)
                .and_then(|i| m.audio_sources.get(i))
                .map(|s| s.id.clone())
        });
        self.close_panel_menu(window, cx);
        if !self.panels_interactive() {
            return;
        }
        if panel == Panel::Audio {
            if index == 1 && !self.model.editable() {
                return;
            }
            if let Some(id) = source_id {
                self.add_recorder_audio_source(id, cx);
                return;
            }
            self.dispatch(
                if index == 0 {
                    Action::Audio
                } else {
                    Action::AudioSettings(AudioSettingsEvent::Refresh)
                },
                window,
                cx,
            );
        } else {
            if let Some(mode) = self.supported_source_modes().get(index).copied() {
                self.dispatch(Action::OpenSourcePicker, window, cx);
                self.dispatch(Action::Source(mode), window, cx);
            }
        }
    }
    fn supported_source_modes(&self) -> Vec<SourceMode> {
        let caps = &self.model.snapshot.capabilities;
        SourceMode::ALL
            .into_iter()
            .filter(|mode| {
                self.model.editable() || matches!(mode, SourceMode::Screen | SourceMode::Window)
            })
            .filter(|mode| match mode {
                SourceMode::Screen => caps.display_capture,
                SourceMode::Window => caps.window_capture,
                SourceMode::Area => caps.region_capture,
                SourceMode::Camera => {
                    caps.camera_overlay
                        && self.model.request.as_ref().is_some_and(|r| {
                            matches!(
                                r.target,
                                CaptureTarget::Display(_) | CaptureTarget::Region(_)
                            )
                        })
                }
            })
            .collect()
    }
    fn panel_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.workspace_ui.menu.as_ref()?;
        let anchor = *self.workspace_ui.anchors.get(&menu.panel)?;
        let audio = menu.panel == Panel::Audio;
        let items = if audio {
            let mut items = vec![
                (
                    "panel-audio-configure".to_owned(),
                    "Configure audio sources".to_owned(),
                ),
                ("panel-audio-refresh".into(), "Refresh audio devices".into()),
            ];
            items.extend(menu.audio_sources.iter().map(|source| {
                (
                    format!("panel-audio-source-{}", source.id.0),
                    source.display_name.clone(),
                )
            }));
            items
        } else {
            self.supported_source_modes()
                .into_iter()
                .map(|mode| {
                    (
                        format!("Source({mode:?})"),
                        match mode {
                            SourceMode::Screen => "Display",
                            SourceMode::Window => "Window",
                            SourceMode::Area => "Area",
                            SourceMode::Camera => "Camera overlay",
                        }
                        .to_owned(),
                    )
                })
                .collect()
        };
        let count = items.len();
        let mut popup = div()
            .id("panel-add-menu")
            .debug_selector(|| "panel-add-menu".into())
            .role(gpui::Role::ListBox)
            .aria_label(if audio {
                "Add audio source"
            } else {
                "Add capture source"
            })
            .track_focus(&menu.focus)
            .w(px(248.))
            .max_h(px(280.))
            .overflow_y_scroll()
            .track_scroll(&menu.scroll)
            .p(px(6.))
            .rounded(px(8.))
            .border_1()
            .border_color(Color::DefaultBorder.get())
            .bg(crate::ui::theme::surface_treatment::overlay_color())
            .shadow(vec![crate::ui::theme::Theme::shadow()])
            .flex()
            .flex_col()
            .gap(px(3.))
            .occlude()
            .on_mouse_down_out(cx.listener(
                move |home, event: &gpui::MouseDownEvent, window, cx| {
                    if !anchor.contains(&event.position) {
                        cx.stop_propagation();
                        home.close_panel_menu(window, cx);
                    }
                },
            ))
            .capture_key_down(cx.listener(move |home, event: &KeyDownEvent, window, cx| {
                let Some(menu) = home.workspace_ui.menu.as_mut() else {
                    return;
                };
                match event.keystroke.key.as_str() {
                    "down" if count > 0 => menu.active = (menu.active + 1) % count,
                    "up" if count > 0 => menu.active = (menu.active + count - 1) % count,
                    "home" => menu.active = 0,
                    "end" if count > 0 => menu.active = count - 1,
                    "enter" | "space" if count > 0 => {
                        let active = menu.active.min(count - 1);
                        home.select_panel_menu(active, window, cx);
                    }
                    "escape" | "tab" => {
                        home.close_panel_menu(window, cx);
                    }
                    _ => return,
                }
                if let Some(menu) = home.workspace_ui.menu.as_ref() {
                    menu.scroll.scroll_to_item(menu.active);
                }
                cx.stop_propagation();
                cx.notify();
            }));
        if count == 0 {
            popup = popup.child(
                Text::new("No supported capture sources available").variant(TextVariant::Caption),
            );
        }
        for (index, (id, label)) in items.into_iter().enumerate() {
            popup = popup.child(
                div().flex_shrink_0().child(
                    Button::new(id, label.clone())
                        .size(ButtonSize::Row)
                        .variant(ButtonVariant::Ghost)
                        .content(div().min_w_0().truncate().child(label))
                        .selected(index == menu.active)
                        .disabled(audio && index == 1 && !self.model.editable())
                        .on_click(cx.listener(move |home, _, window, cx| {
                            home.select_panel_menu(index, window, cx)
                        })),
                ),
            );
        }
        Some(
            gpui::deferred(
                gpui::anchored()
                    .position(gpui::point(
                        anchor.right() - px(248.),
                        anchor.bottom() + px(4.),
                    ))
                    .snap_to_window_with_margin(px(8.))
                    .child(popup),
            )
            .with_priority(3)
            .into_any_element(),
        )
    }
    pub(super) fn workspace_width(&self, panel: Panel) -> f32 {
        (self.workspace_ui.geometry.pane(panel).width - self.workspace_ui.padding * 2. - 2.).max(1.)
    }
    pub(super) fn prepare_workspace(&mut self, width: f32, height: f32, cx: &gpui::App) {
        let (margin, gap, padding) = {
            let _ = cx;
            (12., 10., 8.)
        };

        let header = 32.;
        let layout = self
            .workspace_ui
            .resize
            .as_ref()
            .map_or(&self.model.preferences.workspace, |resize| &resize.layout);
        let viewport_height = (height - 48. - margin * 2.).max(1.);
        self.workspace_ui.geometry =
            layout.geometry((width - margin * 2.).max(1.), viewport_height, gap);
        self.workspace_ui.viewport_height = viewport_height;
        self.workspace_ui.margin = margin;
        self.workspace_ui.gap = gap;
        self.workspace_ui.padding = padding;
        self.workspace_ui.header = header;
    }
    fn save_workspace(&mut self, cx: &mut Context<Self>) {
        self.persist_appearance(cx);
    }
    pub(super) fn reset_workspace(&mut self, cx: &mut Context<Self>) {
        self.model.preferences.workspace = DockLayout::default();
        self.save_workspace(cx);
        cx.notify();
    }
    pub(super) fn workspace_escape(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        event.keystroke.key == "escape" && self.cancel_workspace_drag(window, cx)
    }
    pub(super) fn cancel_workspace_drag(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.workspace_ui.dragging.is_none() && self.workspace_ui.resize.is_none() {
            return false;
        }
        self.workspace_ui.resize = None;
        self.workspace_ui.dragging = None;
        self.workspace_ui.target = None;
        cx.stop_active_drag(window);
        cx.stop_propagation();
        cx.notify();
        true
    }
    fn panel_drag_target(&mut self, event: &DragMoveEvent<PanelDrag>, cx: &mut Context<Self>) {
        if event.drag(cx).owner != cx.entity_id() || self.workspace_ui.dragging.is_none() {
            return;
        }
        let p = event.event.position - event.bounds.origin;
        let (x, y) = (
            f32::from(p.x) - self.workspace_ui.margin,
            f32::from(p.y) - self.workspace_ui.margin,
        );
        let target = self
            .workspace_ui
            .geometry
            .panes
            .iter()
            .find_map(|(panel, r)| {
                (*panel != event.drag(cx).panel
                    && x >= r.x
                    && x <= r.x + r.width
                    && y >= r.y
                    && y <= r.y + r.height)
                    .then(|| (*panel, target_edge(*r, x, y)))
            });
        if target != self.workspace_ui.target {
            self.workspace_ui.target = target;
            cx.notify();
        }
    }
    fn resize_workspace(&mut self, event: &DragMoveEvent<SplitDrag>, cx: &mut Context<Self>) {
        if event.drag(cx).owner != cx.entity_id() || self.workspace_ui.resize.is_none() {
            return;
        }
        let Some(split) = self
            .workspace_ui
            .geometry
            .splits
            .iter()
            .find(|split| split.path == event.drag(cx).split.path)
        else {
            return;
        };
        let p = event.event.position - event.bounds.origin;
        let (position, origin) = match split.axis {
            Axis::Horizontal => (f32::from(p.x) - self.workspace_ui.margin, split.parent.x),
            Axis::Vertical => (f32::from(p.y) - self.workspace_ui.margin, split.parent.y),
        };
        let resize = self.workspace_ui.resize.as_mut().unwrap();
        let ratio = split
            .ratio_for_cut(position - origin - self.workspace_ui.gap / 2. - resize.grab_offset);
        if ratio != split.ratio_for_cut(split.cut()) && resize.layout.resize(&split.path, ratio) {
            cx.notify();
        }
    }
    pub(super) fn docked_workspace(
        &mut self,
        capture: CaptureStage,
        footer: impl IntoElement,
        panel_backgrounds: &PanelBackgrounds,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let geometry = self.workspace_ui.geometry.clone();
        let margin = self.workspace_ui.margin;
        let padding = self.workspace_ui.padding;
        let header_height = self.workspace_ui.header;
        let editable = self.preferences_loaded && !self.model.closing;
        let owner = cx.entity_id();

        let inspect = false;
        let accent = Color::Accent.get();
        let preview = geometry.pane(Panel::Preview);
        let body = Rect {
            x: 0.,
            y: 0.,
            width: (preview.width - padding * 2.).max(1.),
            height: (preview.height - header_height - padding * 2.).max(1.),
        };
        let fitted = body.fit(self.model.source_dimensions().unwrap_or((16, 9)));
        let source_editable = self.model.editable();
        let capture = div()
            .id("capture-source-drop")
            .debug_selector(|| "capture-source-drop".into())
            .size_full()
            .relative()
            .on_mouse_move(cx.listener(|home, _, _window, cx| {
                #[cfg(target_os = "windows")]
                if let Some(preview) = &home.preview
                    && !preview.read(cx).sleeping()
                {
                    preview.update(cx, |p, _| p.note_interaction());
                }
            }))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|home, _, window, cx| {
                    #[cfg(target_os = "windows")]
                    if let Some(preview) = &home.preview {
                        preview.update(cx, |p, cx| p.interact(window, cx));
                    }
                }),
            )
            .can_drop(move |value, _, _| source_editable && shell::can_place_source(value, owner))
            .on_drop(cx.listener(|home, drag: &shell::ListDrag, window, cx| {
                home.place_source(drag, window, cx);
            }))
            .child(
                box_at(fitted)
                    .id("recorder-picture")
                    .debug_selector(|| "recorder-picture".into())
                    .child(capture),
            )
            .child(self.camera_editor(fitted, body.width, body.height, cx))
            .into_any_element();
        let audio = self.home_audio_panel(cx);
        let display = self.source_shelf(cx);
        let controls = self
            .controls(cx)
            .command_deck(self.workspace_width(Panel::Controls));
        // Headless fixtures never sample the host; the actual development and
        // production windows both use real process measurements.
        #[cfg(not(test))]
        if self.workspace_ui.resources.is_none() {
            self.workspace_ui.resources =
                Some(cx.new(|cx| {
                    crate::ui::components::resource_usage::ResourceUsage::new(window, cx)
                }));
        }
        if let Some(resources) = &self.workspace_ui.resources {
            resources.update(cx, |view, cx| {
                view.recording(
                    matches!(
                        self.model.snapshot.state,
                        RecorderState::Recording | RecorderState::Paused
                    ),
                    cx,
                )
            });
            if self.workspace_ui.memory_graph.is_none() {
                self.workspace_ui.memory_graph = Some(cx.new(|cx| {
                    crate::ui::components::resource_usage::MemoryGraph::new(resources.clone(), cx)
                }));
            }
        }
        let graph = self.workspace_ui.memory_graph.as_ref().map_or_else(
            || crate::ui::components::resource_usage::empty_graph().into_any_element(),
            |view| view.clone().into_any_element(),
        );
        let resources = self.workspace_ui.resources.as_ref().map_or_else(
            || crate::ui::components::resource_usage::strip(Default::default()).into_any_element(),
            |view| view.clone().into_any_element(),
        );
        let controls_body_height =
            (geometry.pane(Panel::Controls).height - header_height - padding * 2. - 2.).max(1.);
        let controls = div()
            .id("command-deck-scroll")
            .debug_selector(|| "command-deck-scroll".into())
            .w_full()
            .h(px(controls_body_height))
            .max_h(px(controls_body_height))
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(div().w_full().min_w_0().flex_shrink_0().child(controls))
            .child(
                div()
                    .id("command-deck-breathing-room")
                    .debug_selector(|| "command-deck-breathing-room".into())
                    .flex_shrink_0()
                    .min_h(px(28.))
                    // Grow spare space while retaining the graph/notification
                    // content height; short docks use the outer scroll area.
                    .flex_grow_1()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(graph)
                    .child(
                        div()
                            .id("command-deck-notifications")
                            .debug_selector(|| "command-deck-notifications".into())
                            .w_full()
                            .flex_shrink_0()
                            .min_h_0()
                            .overflow_y_scroll()
                            .child(self.recording_notifications(cx)),
                    ),
            )
            .child(div().mt_auto().flex_shrink_0().child(footer))
            .child(self.output_destination(window, cx))
            .into_any_element();
        let mut contents = std::collections::BTreeMap::from([
            (Panel::Preview, capture),
            (Panel::Audio, audio.into_any_element()),
            (Panel::Display, display),
            (Panel::Controls, controls),
            (Panel::Resources, resources),
        ]);
        let mut workspace = div()
            .id("workspace")
            .debug_selector(|| "workspace".into())
            .relative()
            .w_full()
            .h(px(geometry.height + margin * 2.))
            .flex_shrink_0()
            .capture_key_down(cx.listener(|home, event, window, cx| {
                home.workspace_escape(event, window, cx);
            }))
            .on_drag_move(
                cx.listener(|home, event: &DragMoveEvent<PanelDrag>, _, cx| {
                    home.panel_drag_target(event, cx)
                }),
            )
            .on_drag_move(
                cx.listener(|home, event: &DragMoveEvent<SplitDrag>, _, cx| {
                    home.resize_workspace(event, cx)
                }),
            )
            .on_drop(cx.listener(|home, drag: &PanelDrag, _, cx| {
                if drag.owner != cx.entity_id() || home.workspace_ui.dragging != Some(drag.panel) {
                    return;
                }
                if let Some((target, edge)) = home.workspace_ui.target.take() {
                    let mut layout = home.workspace_ui.geometry.layout.clone();
                    if layout.dock(drag.panel, target, edge) {
                        home.model.preferences.workspace = layout;
                        home.save_workspace(cx);
                    }
                }
                home.workspace_ui.dragging = None;
                cx.notify();
            }));
        for panel in Panel::ALL {
            let rect = geometry.pane(panel);
            let rect = Rect {
                x: rect.x + margin,
                y: rect.y + margin,
                ..rect
            };
            let drag_home = cx.weak_entity();
            let header_key = format!("dock-header-{}", panel.key());
            let group = header_key.clone();
            let header_focus = window
                .use_keyed_state(format!("dock-focus-{}", panel.key()), cx, |_, cx| {
                    cx.focus_handle().tab_stop(true)
                })
                .read(cx)
                .clone();
            let show_grip =
                self.workspace_ui.dragging == Some(panel) || header_focus.is_focused(window);
            let mut header = div()
                .id(header_key.clone())
                .group(group.clone())
                .track_focus(&header_focus)
                .tab_index(0)
                .focus_visible(|d| d.border_color(Color::ActiveBorder.get()))
                .debug_selector(move || header_key)
                .w_full()
                .h(px(header_height))
                .flex_shrink_0()
                .px(px(12.))
                .flex()
                .items_center()
                .gap(px(8.))
                .text_size(px(13.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Color::PrimaryText.get())
                .bg(crate::ui::theme::surface_treatment::dock_header_color())
                .border_b_1()
                .border_color(Color::SubtleBorder.get())
                .child(
                    div()
                        .id(format!("dock-grip-{}", panel.key()))
                        .opacity(if show_grip { 1. } else { 0. })
                        .group_hover(group.clone(), |d| d.opacity(1.))
                        .group_active(group, |d| d.opacity(1.))
                        .child(Icon::new(Icons::Grip).size(IconSize::Small)),
                )
                .child(div().flex_1().min_w_0().truncate().child(panel.title()))
                .when(editable && !inspect, |d| {
                    d.cursor(CursorStyle::OpenHand).on_drag(
                        PanelDrag { owner, panel },
                        move |drag, _, _, cx| {
                            let _ = drag_home.update(cx, |home, cx| {
                                home.workspace_ui.dragging = Some(drag.panel);
                                home.workspace_ui.target = None;
                                cx.notify();
                            });
                            let weak = drag_home.clone();
                            cx.new(|cx| {
                                cx.on_release(move |_, cx| {
                                    cx.defer(move |cx| {
                                        let _ = weak.update(cx, |home, cx| {
                                            if home.workspace_ui.dragging.take().is_some() {
                                                home.workspace_ui.target = None;
                                                cx.notify();
                                            }
                                        });
                                    })
                                })
                                .detach();
                                drag.clone()
                            })
                        },
                    )
                });
            if matches!(panel, Panel::Audio | Panel::Display) {
                let audio = panel == Panel::Audio;
                let measure = cx.weak_entity();
                header = header.child(
                    div()
                        .relative()
                        .size(px(28.))
                        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(
                            Button::new(
                                if audio { "add-audio" } else { "add-source" },
                                if audio {
                                    "Add Source"
                                } else {
                                    "Add display or window"
                                },
                            )
                            .tooltip(if audio {
                                "Add audio source"
                            } else {
                                "Add capture source"
                            })
                            .icon(Icons::Plus)
                            .variant(ButtonVariant::Ghost)
                            .size(ButtonSize::ListIcon)
                            .disabled(!self.panels_interactive())
                            .on_click(cx.listener(
                                move |home, _, window, cx| home.open_panel_menu(panel, window, cx),
                            )),
                        )
                        .child(
                            gpui::canvas(
                                move |bounds, _, cx| {
                                    let _ = measure.update(cx, |home, _| {
                                        home.workspace_ui.anchors.insert(panel, bounds);
                                    });
                                },
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .inset_0()
                            .size_full(),
                        ),
                );
            }
            let header = crate::ui::finish_design(
                crate::ui::designable(header, "workspace-headers", "Panel headers", cx)
                    .h(px(header_height)),
                "workspace-headers",
                cx,
            );
            let content = contents.remove(&panel).unwrap();
            let mut pane = box_at(rect)
                .id(format!("dock-panel-{}", panel.key()))
                .debug_selector(move || format!("dock-panel-{}", panel.key()))
                .rounded(px(10.))
                .border_1()
                .border_color(Color::DefaultBorder.get().opacity(0.65))
                .overflow_hidden()
                .flex()
                .flex_col()
                .min_w_0()
                .min_h_0()
                .when(panel != Panel::Preview, |d| d.bg(Color::DockSurface.get()))
                .when(
                    panel != Panel::Preview && crate::ui::theme::surface_treatment::is_frosted(),
                    |d| {
                        let radius = 10.;
                        d.child(panel_backgrounds.track(Some(px(radius))))
                    },
                )
                .child(header)
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .p(px(padding))
                        .child(content),
                );
            if self.workspace_ui.dragging == Some(panel) {
                pane = pane.border_color(accent).opacity(0.65);
            }
            // Geometry stays owned by docking, even while inspecting cosmetic overrides.
            let pane = crate::ui::designable(pane, "workspace-panels", "Dock panel surfaces", cx)
                .left(px(rect.x))
                .top(px(rect.y))
                .w(px(rect.width))
                .h(px(rect.height))
                .p_0()
                .when(panel == Panel::Preview, |d| {
                    d.bg(gpui::rgba(0x00000000))
                        .rounded(px(0.))
                        .border_0()
                        .shadow(vec![])
                })
                .when(panel == Panel::Controls, |d| {
                    d.relative()
                        .top_0()
                        .left_0()
                        .h(px(rect.height))
                        .max_h(px(rect.height.max(1.)))
                        .flex_shrink_0()
                });
            let pane = crate::ui::finish_design(pane, "workspace-panels", cx);
            workspace = workspace.child(if panel == Panel::Controls {
                box_at(rect)
                    .id("command-deck-stack")
                    .debug_selector(|| "command-deck-stack".into())
                    .flex()
                    .flex_col()
                    .child(pane)
                    .into_any_element()
            } else {
                pane.into_any_element()
            });
        }
        for split in &geometry.splits {
            let r = split.bounds;
            let drag_home = cx.weak_entity();
            let split = split.clone();
            let label = format!(
                "dock-split-{}",
                split
                    .path
                    .iter()
                    .map(|b| if *b { '1' } else { '0' })
                    .collect::<String>()
            );
            let cursor = if split.axis == Axis::Horizontal {
                CursorStyle::ResizeLeftRight
            } else {
                CursorStyle::ResizeUpDown
            };
            let path = split.path.clone();
            workspace = workspace.child(
                box_at(Rect {
                    x: r.x + margin,
                    y: r.y + margin,
                    ..r
                })
                .id(label.clone())
                .debug_selector(move || label)
                .rounded(px(4.))
                .hover(move |d| d.bg(accent.opacity(0.20)))
                .role(gpui::Role::Button)
                .aria_label("Resize dock panels")
                .when(editable && !inspect, |d| {
                    d.cursor(cursor)
                        .focusable()
                        .tab_index(0)
                        .focus_visible(|d| d.bg(accent.opacity(0.25)))
                        .on_key_down(cx.listener(move |home, event: &KeyDownEvent, _, cx| {
                            let Some(split) = home
                                .workspace_ui
                                .geometry
                                .splits
                                .iter()
                                .find(|s| s.path == path)
                            else {
                                return;
                            };
                            let delta = match (split.axis, event.keystroke.key.as_str()) {
                                (Axis::Horizontal, "left") | (Axis::Vertical, "up") => -0.025,
                                (Axis::Horizontal, "right") | (Axis::Vertical, "down") => 0.025,
                                _ => return,
                            };
                            let ratio =
                                split.ratio_for_cut(split.cut() + split.available() * delta);
                            let mut layout = home.workspace_ui.geometry.layout.clone();
                            if ratio != split.ratio_for_cut(split.cut())
                                && layout.resize(&path, ratio)
                            {
                                home.model.preferences.workspace = layout;
                                home.save_workspace(cx);
                                cx.notify();
                            }
                            cx.stop_propagation();
                        }))
                        .on_drag(SplitDrag { owner, split }, move |drag, offset, _, cx| {
                            let _ = drag_home.update(cx, |home, cx| {
                                let initial = home.workspace_ui.geometry.layout.clone();
                                home.workspace_ui.resize = Some(ResizeSession {
                                    layout: initial.clone(),
                                    initial,
                                    grab_offset: match drag.split.axis {
                                        Axis::Horizontal => f32::from(offset.x),
                                        Axis::Vertical => f32::from(offset.y),
                                    } - home.workspace_ui.gap / 2.,
                                });
                                cx.notify();
                            });
                            let weak = drag_home.clone();
                            cx.new(|cx| {
                                cx.on_release(move |_, cx| {
                                    cx.defer(move |cx| {
                                        let _ = weak.update(cx, |home, cx| {
                                            if let Some(resize) = home.workspace_ui.resize.take() {
                                                if resize.initial != resize.layout {
                                                    home.model.preferences.workspace =
                                                        resize.layout;
                                                    home.save_workspace(cx);
                                                }
                                                cx.notify();
                                            }
                                        });
                                    })
                                })
                                .detach();
                                drag.clone()
                            })
                        })
                }),
            );
        }
        if let Some(source) = self.workspace_ui.dragging {
            for (panel, rect) in &geometry.panes {
                if *panel == source {
                    continue;
                }
                let rect = Rect {
                    x: rect.x + margin,
                    y: rect.y + margin,
                    ..*rect
                };
                workspace = workspace.child(
                    box_at(rect)
                        .rounded(px(10.))
                        .border_1()
                        .border_color(accent.opacity(0.35))
                        .bg(accent.opacity(0.025)),
                );
                let (center_x, center_y) = (rect.x + rect.width / 2., rect.y + rect.height / 2.);
                for (edge, dx, dy, glyph) in [
                    (Edge::Left, -32., 0., "←"),
                    (Edge::Right, 32., 0., "→"),
                    (Edge::Top, 0., -32., "↑"),
                    (Edge::Bottom, 0., 32., "↓"),
                    (Edge::Swap, 0., 0., "↔"),
                ] {
                    workspace = workspace.child(
                        div()
                            .id(format!("dock-target-{}-{}", panel.key(), edge.key()))
                            .debug_selector({
                                let p = *panel;
                                move || format!("dock-target-{}-{}", p.key(), edge.key())
                            })
                            .absolute()
                            .left(px(center_x + dx - 12.))
                            .top(px(center_y + dy - 12.))
                            .size(px(24.))
                            .rounded(px(4.))
                            .bg(Color::ElevatedSurface.get())
                            .text_color(accent)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(glyph),
                    );
                }
            }
            if let Some((panel, edge)) = self.workspace_ui.target {
                let mut layout = geometry.layout.clone();
                layout.dock(source, panel, edge);
                let r = layout
                    .geometry(
                        geometry.width,
                        self.workspace_ui.viewport_height,
                        self.workspace_ui.gap,
                    )
                    .pane(source);
                workspace = workspace.child(
                    box_at(Rect {
                        x: r.x + margin,
                        y: r.y + margin,
                        ..r
                    })
                    .id("dock-drop-preview")
                    .debug_selector(|| "dock-drop-preview".into())
                    .rounded(px(8.))
                    .border_1()
                    .border_color(accent)
                    .bg(accent.opacity(0.13)),
                );
            }
        }
        if let Some(menu) = self.panel_menu(cx) {
            workspace = workspace.child(menu);
        }
        if let Some(menu) = self.source_menu(cx) {
            workspace = workspace.child(menu);
        }
        crate::ui::finish_design(
            crate::ui::designable(workspace, "workspace", "Workspace spacing", cx)
                .w_full()
                .h(px(geometry.height + margin * 2.))
                .p_0(),
            "workspace",
            cx,
        )
    }
}
