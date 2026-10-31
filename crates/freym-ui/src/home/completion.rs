use super::*;
use crate::ui::{overlays::Modal, primitives::*, theme::colors::Color};
use gpui::div;
use std::path::{Path, PathBuf};

pub(super) struct RecordingCompletion {
    generation: u64,
    artifact: RecordingArtifact,
    restore: Option<FocusHandle>,
}

impl Home {
    pub(super) fn completion_visible(&self) -> bool {
        self.recording_completion.is_some()
            && self.model.snapshot.state == RecorderState::Idle
            && !self.model.closing
            && self.recording_error().is_none()
            && self.save_state.leave.is_none()
    }

    pub(super) fn present_recording_completion(
        &mut self,
        artifact: RecordingArtifact,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.completion_generation = self.completion_generation.wrapping_add(1);
        let restore = self
            .recording_completion
            .as_ref()
            .and_then(|completion| completion.restore.clone())
            .or_else(|| window.focused(cx));
        self.recording_completion = Some(RecordingCompletion {
            generation: self.completion_generation,
            artifact,
            restore,
        });
        cx.notify();
    }

    pub(super) fn dismiss_recording_completion(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(completion) = self.recording_completion.take() {
            if let Some(restore) = completion.restore {
                window.focus(&restore, cx);
            }
            cx.notify();
        }
    }

    fn open_completion_path(&self, path: &Path, cx: &mut Context<Self>) {
        if self.model.closing {
            return;
        }
        #[cfg(test)]
        shell::OPENED_OUTPUT
            .with(|opened| *opened.borrow_mut() = Some(path.to_string_lossy().into_owned()));
        #[cfg(not(test))]
        cx.open_with_system(path);
        #[cfg(test)]
        let _ = cx;
    }

    pub(super) fn recording_completion_popup(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(completion) = &self.recording_completion else {
            return div().into_any_element();
        };
        let generation = completion.generation;
        // Finalized supplies the actual output path; no file scan is needed here.
        let path = PathBuf::from(&completion.artifact.path);
        let filename = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let label = path.to_string_lossy().into_owned();
        let folder = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf);
        Modal::new(
            format!("recording-completion-{generation}"),
            "Recording saved",
        )
        .width(560.)
        .compact()
        .on_dismiss(cx.listener(move |this, _, window, cx| {
            if this
                .recording_completion
                .as_ref()
                .is_some_and(|completion| completion.generation == generation)
            {
                this.dismiss_recording_completion(window, cx);
            }
        }))
        .child(
            div()
                .id("recording-completion-file")
                .debug_selector(|| "recording-completion-file".into())
                .w_full()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Text::new(filename).variant(TextVariant::Label).truncate())
                        .child(
                            Button::new("completion-open-recording", label.clone())
                                .size(ButtonSize::Small)
                                .variant(ButtonVariant::Ghost)
                                .fill_width()
                                .tooltip(label.clone())
                                .content(
                                    Text::new(label)
                                        .variant(TextVariant::Caption)
                                        .color(Color::SecondaryText)
                                        .truncate(),
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_completion_path(&path, cx);
                                })),
                        ),
                )
                .child(
                    Button::new("completion-open-folder", "Open location")
                        .size(ButtonSize::ListIcon)
                        .variant(ButtonVariant::Ghost)
                        .tooltip("Open location")
                        .icon(Icons::Folder)
                        .disabled(folder.is_none())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(folder) = &folder {
                                this.open_completion_path(folder, cx);
                            }
                        })),
                ),
        )
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext};

    #[gpui::test]
    fn completion_actions_use_the_result_and_dismiss_without_growing_the_deck(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::bind_keys);
        let (view, cx) = cx.add_window_view(super::super::tests::home);
        cx.run_until_parked();
        let deck = cx.debug_bounds("dock-panel-controls").unwrap();
        let path = PathBuf::from("C:/recordings/actual-output.mkv");
        cx.update(|window, cx| {
            view.update(cx, |home, cx| {
                let artifact = RecordingArtifact {
                    path: path.to_string_lossy().into_owned(),
                    duration_ms: 12_345.,
                    audio_tracks: 2,
                };
                home.model.snapshot.last_artifact = Some(artifact.clone());
                home.model.snapshot.state = RecorderState::Finalizing;
                home.recording_completion = Some(RecordingCompletion {
                    generation: 1,
                    artifact,
                    restore: window.focused(cx),
                });
                assert!(!home.completion_visible());
                home.model.snapshot.state = RecorderState::Idle;
                assert!(home.completion_visible());
                cx.notify();
            })
        });
        cx.run_until_parked();
        let popup = cx.debug_bounds("modal-panel").unwrap();
        assert!(popup.size.width > popup.size.height * 3.);
        assert!(popup.size.height < px(120.));
        assert!(cx.debug_bounds("completion-details").is_none());
        for (id, expected) in [
            ("completion-open-folder", PathBuf::from("C:/recordings")),
            ("completion-open-recording", path),
        ] {
            let button = cx.debug_bounds(id).unwrap().center();
            cx.simulate_click(button, Modifiers::default());
            cx.run_until_parked();
            shell::OPENED_OUTPUT.with(|opened| {
                assert_eq!(opened.borrow().as_deref(), Some(expected.to_str().unwrap()))
            });
        }
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(view.read_with(cx, |home, _| home.recording_completion.is_none()));
        cx.update(|_, cx| view.update(cx, |_, cx| cx.notify()));
        cx.run_until_parked();
        assert!(cx.debug_bounds("completion-open-recording").is_none());
        assert!(cx.debug_bounds("recording-saved-notification").is_none());
        assert_eq!(cx.debug_bounds("dock-panel-controls").unwrap(), deck);
    }
}
