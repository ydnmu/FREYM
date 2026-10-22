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
        self.render_count += 1;
        if self.compact.is_some() {
            return div().id("compact-owner").size_full();
        }
        let editable = self.model.editable();
        #[cfg(feature = "ui-preview")]
        let workbench = self.ui_preview;
        #[cfg(not(feature = "ui-preview"))]
        let workbench = false;
        let footer_count = usize::from(
            self.model.snapshot.last_artifact.is_some()
                || self.model.preferences.last_recording.is_some(),
        ) + usize::from(
            self.model.snapshot.last_replay.is_some()
                || self.model.preferences.last_replay.is_some(),
        ) + usize::from(self.model.snapshot.replay_active)
            + usize::from(!self.model.interrupted.is_empty());
        let bottom_reserve = if footer_count == 0 {
            0.
        } else {
            footer_count.div_ceil(2) as f32 * 44. + 16.
        };
        let mut footer_actions = Stack::horizontal().gap(Spacing::Xs);
        if self.model.snapshot.last_artifact.is_some()
            || self.model.preferences.last_recording.is_some()
        {
            footer_actions = footer_actions.child(self.button(
                Action::StudioRecording,
                "Edit recording",
                None,
                false,
                !self.model.closing,
                cx,
            ));
        }
        if self.model.snapshot.last_replay.is_some() || self.model.preferences.last_replay.is_some()
        {
            footer_actions = footer_actions.child(self.button(
                Action::StudioReplay,
                "Edit replay",
                None,
                false,
                !self.model.closing,
                cx,
            ));
        }
        if self.model.snapshot.replay_active {
            footer_actions = footer_actions.child(self.button(
                Action::SaveReplay,
                self.model.replay_save_label(),
                None,
                false,
                self.model.can_save_replay(),
                cx,
            ));
        }
        if !self.model.interrupted.is_empty() {
            footer_actions = footer_actions.child(self.button(
                Action::Recovery,
                "Recover",
                None,
                false,
                !self.model.recovery_busy,
                cx,
            ));
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
            RecorderState::Failed(_) => "Recording failed",
        };
        let mut titlebar = Titlebar::new(crate::ui::components::titlebar::close_button(
            cx.listener(|this, _, w, cx| this.close(w, cx)),
        ))
        .actions(
            Stack::horizontal().gap(Spacing::Xxs).child(
                IconButton::new("more-options", Icons::More, "More options")
                    .on_click(self.handler(Action::MoreOptions, cx)),
            ),
        );
        if !state_label.is_empty() {
            titlebar = titlebar.status(Text::new(state_label).variant(TextVariant::Caption).color(
                if self.model.snapshot.state == RecorderState::Recording {
                    Color::Record
                } else {
                    Color::SecondaryText
                },
            ));
        }
        let native_preview = cfg!(target_os = "windows")
            && !workbench
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
                self.model
                    .request
                    .as_ref()
                    .map(|r| (r.target.clone(), r.show_cursor))
            } else {
                None
            };
            let dimensions = self.model.source_dimensions().unwrap_or((16, 9));
            // GPUI overlays compose above the child HWND; keep the picture behind
            // source/settings sheets instead of replacing it with the artwork.
            let exposed = !self.model.closing && self.model.sheet != Some(Sheet::Settings);
            preview.update(cx, |preview, cx| {
                preview.configure(desired, dimensions, exposed, window, cx)
            });
            native_viewport = preview.read(cx).hole(window.scale_factor());
        }
        let explanation = if workbench {
            "UI preview · Example source"
        } else if self.model.connecting {
            "Discovering capture sources…"
        } else if native_preview {
            "Connecting to your selected source…"
        } else {
            "Choose an available source to preview."
        };
        let mut capture = CaptureStage::new(
            self.model.source_label(),
            explanation,
            Button::new("open-live-preview", "Retry preview")
                .size(ButtonSize::Compact)
                .variant(ButtonVariant::Secondary)
                .icon(Icons::Play)
                .disabled(
                    !native_preview
                        || self.model.command_pending
                        || !matches!(
                            self.model.snapshot.state,
                            RecorderState::Idle | RecorderState::Recording | RecorderState::Paused
                        ),
                )
                .on_click(self.handler(Action::Preview, cx)),
            Button::new("pick-window", "Select window")
                .size(ButtonSize::Compact)
                .variant(ButtonVariant::Secondary)
                .icon(Icons::Window)
                .disabled(!editable || !self.model.snapshot.capabilities.window_capture)
                .on_click(self.handler(Action::PickWindow, cx)),
        );
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
                        .child(Text::new(error.to_owned()).variant(TextVariant::Muted))
                        .child(
                            Button::new("retry-preview", "Retry preview")
                                .on_click(self.handler(Action::Preview, cx)),
                        ),
                );
            } else {
                capture = capture.embedded(preview.clone());
            }
        }
        let camera = self.model.request.as_ref().and_then(|r| r.camera.as_ref());
        let camera_label = camera
            .map(|c| {
                self.model
                    .cameras
                    .iter()
                    .find(|d| d.id == c.device_id)
                    .map(|d| d.name.clone())
                    .unwrap_or_else(|| "Selected camera".into())
            })
            .unwrap_or_else(|| "Off".into());
        let fps_label = self
            .model
            .request
            .as_ref()
            .map(|r| match r.fps_mode {
                FpsMode::AutoMatchSource => "Auto FPS".into(),
                FpsMode::Fixed(fps) => format!("{fps} FPS"),
            })
            .unwrap_or_else(|| "Auto FPS".into());
        let video_label = format!(
            "{} · {fps_label}",
            self.model
                .source_dimensions()
                .map(|(w, h)| format!("{w} × {h}"))
                .unwrap_or_else(|| "Native".into())
        );
        let quality_label = self
            .model
            .request
            .as_ref()
            .map(|r| format!("{:?}", r.quality))
            .unwrap_or_else(|| "Automatic".into());
        let sections = &self.model.preferences.recorder_sections;
        let recorder_geometry = RecorderGeometry::new(
            f32::from(window.viewport_size().width),
            f32::from(window.viewport_size().height)
                - if workbench { 100. } else { 0. }
                - bottom_reserve,
        );
        let cards = [
            RecorderSettingCard::new("source-settings", "Video", video_label, Icons::Screen)
                .row_width(recorder_geometry.left)
                .origin(
                    (f32::from(window.viewport_size().width) - recorder_geometry.width) / 2.,
                    72. + recorder_geometry.media + recorder_geometry.row_gap,
                )
                .on_dismiss(cx.listener(|this, _: &(), window, cx| {
                    if this.model.preferences.recorder_sections.video {
                        this.dispatch(Action::ToggleSection(RecorderSection::Video), window, cx);
                    }
                }))
                .expanded(sections.video)
                .controls(self.inline_video(cx))
                .on_click(self.handler(Action::ToggleSection(RecorderSection::Video), cx))
                .into_any_element(),
            RecorderSettingCard::new("camera-settings", "Camera", camera_label, Icons::Camera)
                .row_width(recorder_geometry.left)
                .origin(
                    (f32::from(window.viewport_size().width) - recorder_geometry.width) / 2.,
                    72. + recorder_geometry.media + recorder_geometry.row_gap,
                )
                .on_dismiss(cx.listener(|this, _: &(), window, cx| {
                    if this.model.preferences.recorder_sections.camera {
                        this.dispatch(Action::ToggleSection(RecorderSection::Camera), window, cx);
                    }
                }))
                .expanded(sections.camera)
                .controls(self.inline_camera(cx))
                .on_click(self.handler(Action::ToggleSection(RecorderSection::Camera), cx))
                .into_any_element(),
            RecorderSettingCard::new("quality-settings", "Quality", quality_label, Icons::Quality)
                .row_width(recorder_geometry.left)
                .origin(
                    (f32::from(window.viewport_size().width) - recorder_geometry.width) / 2.,
                    72. + recorder_geometry.media + recorder_geometry.row_gap,
                )
                .on_dismiss(cx.listener(|this, _: &(), window, cx| {
                    if this.model.preferences.recorder_sections.quality {
                        this.dispatch(Action::ToggleSection(RecorderSection::Quality), window, cx);
                    }
                }))
                .expanded(sections.quality)
                .controls(self.inline_quality(cx))
                .on_click(self.handler(Action::ToggleSection(RecorderSection::Quality), cx))
                .into_any_element(),
        ];
        let mut screen = RecorderScreen::new(titlebar, footer)
            .preview_workbench(workbench)
            .bottom_reserve(bottom_reserve)
            .native_viewport(native_viewport)
            .artwork(self.model.preferences.artwork)
            .media(capture, self.home_audio_panel(cx))
            .cards(cards)
            .child(self.controls(cx));
        let mut messages = Stack::vertical().gap(Spacing::Xs);
        let recording_error = match &self.model.snapshot.state {
            RecorderState::Failed(error) => Some(format!("{} {}", error.message, error.action)),
            _ => None,
        };
        if let Some(error) = self.model.error.as_ref().or(recording_error.as_ref()) {
            messages = messages.child(
                div()
                    .h(px(44.))
                    .px(Spacing::Sm.px())
                    .rounded(px(8.))
                    .bg(Color::ElevatedSurface.get())
                    .flex()
                    .items_center()
                    .child(
                        Text::new(error.clone())
                            .variant(TextVariant::Caption)
                            .color(Color::Error)
                            .truncate(),
                    ),
            );
        }
        if let Some(notice) = &self.model.notice {
            messages = messages.child(
                div().w_full().min_w_0().p(Spacing::Xs.px()).child(
                    Text::new(notice.clone())
                        .variant(TextVariant::Caption)
                        .color(Color::Warning),
                ),
            );
        }
        if let Some(artifact) = &self.model.snapshot.last_artifact {
            messages = messages.child(
                gpui::div()
                    .h(px(44.))
                    .px(Spacing::Sm.px())
                    .rounded(px(8.))
                    .bg(Color::ElevatedSurface.get())
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        Text::new(format!(
                            "Recording saved · {}",
                            format_elapsed(artifact.duration_ms)
                        ))
                        .variant(TextVariant::Caption),
                    )
                    .child(
                        Button::new("show-recording-file", "Show file")
                            .size(ButtonSize::Compact)
                            .icon(Icons::Folder)
                            .on_click(self.handler(Action::RevealRecording, cx)),
                    ),
            );
        }
        screen = screen.messages(messages);
        if let Some(editor) = &self.number_editor {
            screen = screen.overlay(self.number_sheet(editor, cx));
        } else if let Some(action) = self.shortcut_edit {
            screen = screen.overlay(
                Modal::new(
                    "shortcut-editor",
                    format!("Shortcut for {}", configuration::shortcut_label(action)),
                )
                .child(
                    Text::new("Press your key combination. Backspace disables it. Esc cancels.")
                        .variant(TextVariant::Caption),
                ),
            );
        } else if let Some(sheet) = self.model.sheet
            && sheet != Sheet::Settings
        {
            screen = screen.overlay(self.sheet(sheet, cx));
        }
        gpui::div()
            .id("freym-window")
            .size_full()
            .track_focus(&self.focus)
            .key_context("FREYM")
            .on_action(cx.listener(|this, _: &Record, w, cx| {
                if this.model.shortcuts.start_stop.is_empty() {
                    this.dispatch(Action::Record, w, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &PauseResume, w, cx| {
                if this.model.shortcuts.pause_resume.is_empty() {
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
            .capture_key_down(cx.listener(Self::capture_shortcut))
            .child(if self.model.sheet == Some(Sheet::Settings) {
                self.settings_page(window, cx)
            } else {
                screen.into_any_element()
            })
    }
}
