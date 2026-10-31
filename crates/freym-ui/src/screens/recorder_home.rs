use super::*;
use crate::ui::{
    components::*,
    overlays::*,
    primitives::*,
    theme::{colors::Color, spacing::Spacing},
};
use gpui::div;

impl Render for Home {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        let _render_timing = super::switch_performance_tests::RenderTiming::start();
        self.install_shortcut_interceptor(cx);
        self.refresh_source_menu(window, cx);
        self.render_count += 1;
        self.workspace_ui
            .set_resources_active(self.model.sheet != Some(Sheet::Settings), cx);

        let native_preview = cfg!(target_os = "windows")
            && self.model.snapshot.capabilities.live_preview
            && self.model.request.as_ref().is_some_and(|r| {
                matches!(
                    r.target,
                    CaptureTarget::Display(_) | CaptureTarget::Region(_) | CaptureTarget::Window(_)
                )
            })
            && (self.model.source_dimensions().is_some()
                || self
                    .model
                    .request
                    .as_ref()
                    .is_some_and(|r| match &r.target {
                        CaptureTarget::Window(id) => {
                            self.model.snapshot.windows.iter().any(|w| &w.id == id)
                        }
                        _ => false,
                    }));
        let mut native_viewport = None;
        #[cfg(target_os = "windows")]
        if let Some(preview) = &self.preview {
            let desired = if native_preview && self.preview_enabled && !self.model.closing {
                self.model.request.as_ref().map(|r| {
                    (
                        r.target.clone(),
                        r.show_cursor,
                        frame_runtime::recording_control::PreviewLayers {
                            camera: r.camera.clone(),
                        },
                    )
                })
            } else {
                None
            };
            let dimensions = self.model.source_dimensions().unwrap_or((16, 9));
            // Keep live pixels exposed during source drags and placement edits.
            // Centered dialogs and panel docking hide the child HWND so it cannot
            // cover their controls; the capture session remains owned by Preview.
            let exposed = !self.model.closing
                && self.workspace_ui.dragging.is_none()
                && !self.panel_menu_open()
                && self.model.sheet.is_none()
                && self.number_editor.is_none()
                && !self.completion_visible();
            preview.update(cx, |preview, cx| {
                preview.set_editing(self.camera_ui.editing && self.model.editable());
                preview.configure(
                    desired,
                    dimensions,
                    exposed,
                    self.model.command_pending
                        || self.model.connecting
                        || self.model.recovery_busy
                        || self.camera_ui.editing
                        || self.camera_ui.loading
                        || self.workspace_ui.source_menu.is_some()
                        || self.workspace_ui.dragging.is_some()
                        || cx.has_active_drag()
                        || self.model.sheet.is_some(),
                    window,
                    cx,
                )
            });
            native_viewport = preview.read(cx).hole(window.scale_factor());
        }
        let content = if self.model.sheet == Some(Sheet::Settings) {
            self.settings_page(window, cx)
        } else {
            let mut footer_actions = div().w_full().flex().flex_wrap().gap(px(6.));
            if self.model.snapshot.replay_active {
                footer_actions = footer_actions.child(
                    Button::new("SaveReplay", self.model.replay_save_label())
                        .size(ButtonSize::Small)
                        .disabled(!self.model.can_save_replay())
                        .on_click(self.handler(Action::SaveReplay, cx)),
                );
            }
            let footer = Stack::vertical().gap(Spacing::Xs).child(footer_actions);
            let state_label = match self.model.snapshot.state {
                _ if self.model.connecting => "Connecting…",
                RecorderState::Idle => "",
                RecorderState::Preparing => "Preparing…",
                RecorderState::Recording if self.model.snapshot.replay_only => "Replay buffering",
                RecorderState::Recording => "Recording",
                RecorderState::Paused => "Paused",
                RecorderState::Finalizing if self.model.snapshot.replay_only => "Stopping replay…",
                RecorderState::Finalizing => "Saving recording…",
                RecorderState::Failed(_) => "",
            };
            let mut titlebar = Titlebar::new(crate::ui::components::titlebar::close_button(
                cx.listener(|this, _, w, cx| this.close(w, cx)),
            ))
            .transparent_background();
            if !state_label.is_empty() {
                titlebar =
                    titlebar.status(Text::new(state_label).variant(TextVariant::Caption).color(
                        if self.model.snapshot.state == RecorderState::Recording {
                            Color::Record
                        } else {
                            Color::SecondaryText
                        },
                    ));
            }
            let explanation: String = if self.model.connecting {
                "Discovering capture sources…".into()
            } else if native_preview {
                "Connecting to your selected source…".into()
            } else {
                "Choose an available source to preview.".into()
            };
            let mut capture = CaptureStage::new(explanation);
            #[cfg(target_os = "windows")]
            if native_preview && let Some(preview) = &self.preview {
                if let Some(error) = preview.read(cx).error() {
                    capture = capture.embedded(
                        div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .p(Spacing::Md.px())
                            .child(Text::new(error.to_owned()).variant(TextVariant::Muted)),
                    );
                } else {
                    let sleeping = preview.read(cx).sleeping();
                    let ready = preview.read(cx).ready();
                    capture =
                        capture.embedded(div().size_full().relative().child(preview.clone()).when(
                            sleeping || !ready,
                            |d| {
                                d.child(
                                    div()
                                        .id("preview-status")
                                        .debug_selector(|| "preview-status".into())
                                        .absolute()
                                        .inset_0()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child(
                                            Text::new(if sleeping {
                                                "Preview sleeping · Click to wake"
                                            } else {
                                                "Connecting live preview…"
                                            })
                                            .variant(TextVariant::Caption),
                                        ),
                                )
                            },
                        ));
                }
            }
            let viewport = crate::ui::viewport(window, cx);
            self.prepare_workspace(viewport.width.into(), f32::from(viewport.height), cx);
            let panel_backgrounds = PanelBackgrounds::default();
            let workspace = self.docked_workspace(capture, footer, &panel_backgrounds, window, cx);
            let mut screen = RecorderScreen::new(titlebar, workspace)
                .native_viewport(native_viewport)
                .panel_backgrounds(panel_backgrounds);
            if let Some(editor) = &self.number_editor {
                screen = screen.overlay(self.number_sheet(editor, cx));
            } else if let Some(action) = self.shortcut_edit {
                screen = screen.overlay(
                    Modal::new(
                        "shortcut-editor",
                        format!("Shortcut for {}", configuration::shortcut_label(action)),
                    )
                    .child(
                        Text::new(
                            "Press your key combination. Backspace disables it. Esc cancels.",
                        )
                        .variant(TextVariant::Caption),
                    ),
                );
            } else if let Some(sheet) = self.model.sheet
                && sheet != Sheet::Settings
            {
                screen = screen.overlay(self.sheet(sheet, cx));
            }
            screen.into_any_element()
        };
        gpui::div()
            .id("freym-window")
            .relative()
            .size_full()
            .track_focus(&self.focus)
            .key_context("FREYM")
            .on_action(cx.listener(|this, _: &Record, w, cx| {
                if this.save_state.shortcuts.start_stop.is_empty() {
                    this.dispatch(Action::Record, w, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &PauseResume, w, cx| {
                if this.save_state.shortcuts.pause_resume.is_empty() {
                    this.dispatch(Action::Pause, w, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Cancel, w, cx| this.cancel(w, cx)))
            .on_action(cx.listener(|this, _: &OpenSettings, w, cx| {
                this.dispatch(Action::Settings, w, cx);
            }))
            .on_action(|_: &Fullscreen, w, _| w.toggle_fullscreen())
            .on_action(|_: &FocusNext, w, cx| w.focus_next(cx))
            .on_action(|_: &FocusPrevious, w, cx| w.focus_prev(cx))
            .capture_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                if window.default_prevented() {
                    this.dispatch_local_shortcut(event.is_held, window, cx);
                    cx.stop_propagation();
                    return;
                }
                this.workspace_escape(event, window, cx);
            }))
            .capture_key_up(cx.listener(|this, event: &gpui::KeyUpEvent, window, cx| {
                this.release_shortcut_capture(&event.keystroke, window, cx);
            }))
            .child(content)
            .when(self.completion_visible(), |d| {
                d.child(self.recording_completion_popup(cx))
            })
            .when_some(self.save_state.leave.clone(), |d, target| {
                d.child(Modal::new("unsaved-settings-dialog", "Unsaved changes")
                    .on_dismiss(cx.listener(|h,_,_,cx| {h.save_state.leave=None;cx.notify();}))
                    .child(Text::new(if target==Action::Exit {"Exit without saving these changes?"} else if target==Action::RetrySettings {"Reload saved settings and discard the staged changes?"} else {"These changes have not been saved. Leave Settings and keep the draft in memory?"}).variant(TextVariant::Caption))
                    .child(div().flex().justify_end().gap(px(8.))
                        .child(Button::new("unsaved-stay","Keep editing").on_click(self.handler(Action::KeepEditing,cx)))
                        .child(Button::new("unsaved-leave",if target==Action::Exit {"Exit without saving"} else if target==Action::RetrySettings {"Reload saved settings"} else {"Leave with draft"}).on_click(self.handler(Action::LeaveUnsaved,cx)))))
            })
            .child(ErrorPopup::new(
                "recorder-error-popup",
                self.recording_error(),
            ))
    }
}

impl Home {
    pub(super) fn recording_error(&self) -> Option<String> {
        self.model
            .error
            .clone()
            .or_else(|| match &self.model.snapshot.state {
                RecorderState::Failed(error) => Some(crate::services::error_details(error)),
                _ => None,
            })
    }
    /// The fixed Command Deck area follows authoritative recorder state. No
    /// notices/debug strings, speculative success, timers or additional polling.
    pub(super) fn recording_notifications(&self, _cx: &mut Context<Self>) -> AnyElement {
        if self.recording_error().is_some() {
            return div().into_any_element();
        }
        if self.model.snapshot.state == RecorderState::Preparing {
            return div()
                .child(
                    crate::ui::components::countdown_picker::RecordingCountdown::new(
                        self.model.countdown_deadline,
                    )
                    .timed_out(self.model.preparation_timed_out),
                )
                .into_any_element();
        }
        let state = match self.model.snapshot.state {
            RecorderState::Recording if self.model.snapshot.replay_only => {
                Some(("recording-started-notification", "Replay buffer started"))
            }
            RecorderState::Recording => {
                Some(("recording-started-notification", "Recording started"))
            }
            RecorderState::Paused => Some(("recording-paused-notification", "Recording paused")),
            RecorderState::Finalizing => {
                Some(("recording-saving-notification", "Saving recording…"))
            }
            _ => None,
        };
        if let Some((id, message)) = state {
            return div()
                .debug_selector(move || id.into())
                .child(Toast::new(message))
                .into_any_element();
        }
        div().into_any_element()
    }
}
