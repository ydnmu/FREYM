//! Recorder row menus use the same guarded actions as the inline controls.
use super::*;
use crate::ui::{primitives::*, theme::colors::Color};
use gpui::{MouseButton, MouseDownEvent, Pixels, Point, div};

#[derive(Clone)]
pub(super) enum SourceRow {
    Camera,
    Audio(AudioSourceId),
}

pub(super) struct SourceMenu {
    row: SourceRow,
    position: Point<Pixels>,
    active: usize,
    focus: FocusHandle,
    previous: Option<FocusHandle>,
}

impl Home {
    pub(super) fn refresh_source_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspace_ui.source_menu.as_ref().is_some_and(|menu| {
            self.completion_visible()
                || self.model.sheet.is_some()
                || self.source_actions(&menu.row).is_empty()
        }) {
            self.close_source_menu(window, cx);
        }
    }
    fn source_actions(&self, row: &SourceRow) -> Vec<(&'static str, Action)> {
        let mut items = Vec::new();
        let editable = self.model.editable();
        match row {
            SourceRow::Camera if editable && self.selected_camera().is_some() => {
                let camera = self.selected_camera().unwrap();
                let available = self.model.cameras.iter().any(|c| c.id == camera.device_id);
                let visible = self
                    .model
                    .request
                    .as_ref()
                    .is_some_and(|r| r.camera.is_some());
                if visible || (available && self.current_camera_format().is_some()) {
                    items.push((
                        if visible { "Hide" } else { "Show" },
                        Action::Video(crate::video::VideoAction::CameraToggle),
                    ));
                }
                if visible {
                    items.push(("Edit position in Preview", Action::CameraPlacement));
                }
                if available && self.camera_ui.settings_supported {
                    items.push(("Camera device settings", Action::CameraDeviceSettings));
                }
                items.push(("Remove", Action::RemoveCamera));
            }
            SourceRow::Audio(id) => {
                let Some(source) = self
                    .recorder_audio_sources()
                    .into_iter()
                    .find(|s| &s.id == id)
                else {
                    return items;
                };
                if editable && !self.model.level_testing {
                    if source.route != AudioRoute::Ignore {
                        items.push((
                            "Disable",
                            Action::SelectRoute(id.clone(), AudioRoute::Ignore),
                        ));
                    } else if source.is_active
                        && (source.kind != AudioSourceKind::Application
                            || self.model.snapshot.capabilities.per_app_audio)
                    {
                        items.push((
                            "Enable",
                            Action::SelectRoute(id.clone(), self.enabled_audio_route(id)),
                        ));
                    }
                    if self.can_remove_recorder_audio_source(id) {
                        items.push((
                            "Remove from Recorder",
                            Action::RemoveRecorderAudio(id.clone()),
                        ));
                    }
                }
                if self.can_mute(id) {
                    items.push((
                        if source.muted { "Unmute" } else { "Mute" },
                        Action::MuteAudio(id.clone(), !source.muted),
                    ));
                }
            }
            _ => {}
        }
        items
    }

    pub(super) fn open_source_menu(
        &mut self,
        row: SourceRow,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if cx.has_active_drag()
            || self.camera_ui.drag.is_some()
            || self.source_actions(&row).is_empty()
        {
            return;
        }
        self.close_panel_menu(window, cx);
        self.close_source_menu(window, cx);
        let focus = cx.focus_handle().tab_stop(false);
        self.workspace_ui.source_menu = Some(SourceMenu {
            row,
            position: event.position,
            active: 0,
            previous: window.focused(cx),
            focus: focus.clone(),
        });
        window.focus(&focus, cx);
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn close_source_menu(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(menu) = self.workspace_ui.source_menu.take() else {
            return false;
        };
        if let Some(previous) = menu.previous {
            window.focus(&previous, cx);
        }
        cx.notify();
        true
    }

    fn select_source_action(
        &mut self,
        action: Action,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Revalidate against current identities/state, including a menu left open at Record.
        let supported = self.workspace_ui.source_menu.as_ref().is_some_and(|menu| {
            self.source_actions(&menu.row)
                .iter()
                .any(|(_, current)| *current == action)
        });
        self.close_source_menu(window, cx);
        if supported {
            self.dispatch(action, window, cx);
        }
    }

    pub(super) fn source_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.workspace_ui.source_menu.as_ref()?;
        let items = self.source_actions(&menu.row);
        if items.is_empty() {
            return None;
        }
        let active = menu.active.min(items.len() - 1);
        let mut popup = div()
            .id("source-context-menu")
            .debug_selector(|| "source-context-menu".into())
            .role(gpui::Role::ListBox)
            .aria_label("Source actions")
            .track_focus(&menu.focus)
            .w(px(224.))
            .p(px(6.))
            .rounded(px(8.))
            .border_1()
            .border_color(Color::DefaultBorder.get())
            .bg(crate::ui::theme::surface_treatment::overlay_color())
            .shadow(vec![crate::ui::theme::Theme::shadow()])
            .flex()
            .flex_col()
            .gap(px(2.))
            .occlude()
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_mouse_down_out(cx.listener(|home, event: &MouseDownEvent, window, cx| {
                home.close_source_menu(window, cx);
                if event.button == MouseButton::Left {
                    cx.stop_propagation();
                }
            }))
            .capture_key_down(cx.listener(|home, event: &gpui::KeyDownEvent, window, cx| {
                let Some(menu) = home.workspace_ui.source_menu.as_ref() else {
                    return;
                };
                let items = home.source_actions(&menu.row);
                let count = items.len();
                if count == 0 {
                    home.close_source_menu(window, cx);
                    return;
                }
                let active = menu.active.min(count - 1);
                let next = match event.keystroke.key.as_str() {
                    "down" => (active + 1) % count,
                    "up" => (active + count - 1) % count,
                    "home" => 0,
                    "end" => count - 1,
                    "enter" | "space" => {
                        home.select_source_action(items[active].1.clone(), window, cx);
                        cx.stop_propagation();
                        return;
                    }
                    "escape" | "tab" => {
                        home.close_source_menu(window, cx);
                        cx.stop_propagation();
                        return;
                    }
                    _ => return,
                };
                if let Some(menu) = home.workspace_ui.source_menu.as_mut() {
                    menu.active = next;
                }
                cx.stop_propagation();
                cx.notify();
            }));
        for (index, (label, action)) in items.into_iter().enumerate() {
            popup = popup.child(
                Button::new(format!("source-context-action-{index}"), label)
                    .size(ButtonSize::Row)
                    .variant(ButtonVariant::Ghost)
                    .fill_width()
                    .selected(index == active)
                    .on_click(cx.listener(move |home, _, window, cx| {
                        cx.stop_propagation();
                        home.select_source_action(action.clone(), window, cx);
                    })),
            );
        }
        Some(
            gpui::deferred(
                gpui::anchored()
                    .position(menu.position)
                    .snap_to_window_with_margin(px(8.))
                    .child(popup),
            )
            .with_priority(4)
            .into_any_element(),
        )
    }
}
