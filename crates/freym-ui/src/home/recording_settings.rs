//! Recording/Performance-owned settings composition and read-only verification.
use super::*;
use crate::ui::{components::*, primitives::*};
use frame_runtime::{recording_files, recording_health};

pub(super) fn validated_filename(value: &str) -> Result<String, String> {
    let stem = value.trim();
    if stem.is_empty() {
        return Ok(frame_runtime::recording_control::AUTO_FILE_STEM.into());
    }
    recording_files::validate_filename_template(stem).map_err(|e| e.message)?;
    Ok(stem.into())
}

impl Home {
    pub(super) fn recording_files_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let stem = self
            .model
            .request
            .as_ref()
            .map(|r| r.output.file_stem.as_str());
        let label = match stem {
            None | Some(frame_runtime::recording_control::AUTO_FILE_STEM) => {
                recording_files::DEFAULT_FILENAME_TEMPLATE
            }
            Some(stem) => stem,
        };
        let button_label = if label.chars().count() > 32 {
            format!("{}…", label.chars().take(29).collect::<String>())
        } else {
            label.to_owned()
        };
        Stack::vertical()
            .child(Text::new("Recording & Files"))
            .child(SettingRow::new("Filename", self.button(Action::RecordingFilename, button_label, None, false, self.model.editable(), cx))
                .description("Tokens: {date}, {time}, {timestamp}. Templates add a suffix on collision; exact names refuse overwrite."))
            .child(Text::new("Format: Matroska (.mkv). H.264, HEVC or AV1 video and Opus audio depend on the selected source, signal and available encoder.").variant(TextVariant::Caption))
            .child(Text::new("Interrupted recording journals and recovery copies are enabled. Preserve the MKV and its session/metadata sidecars together.").variant(TextVariant::Caption))
            .child(Text::new("Storage failure: stop capture and preserve incomplete files. Continuing after a failed write is unavailable.").variant(TextVariant::Caption))
            .child(Text::new(if cfg!(target_os = "windows") {
                "Disk space: warn below 1 GiB available to this user, checked every 5 seconds. Available space is an observation, not a recording-time guarantee."
            } else { "Disk space: this platform has no connected volume probe." }).variant(TextVariant::Caption))
            .child(Text::new("Timed/size recording splits: unavailable. Replay uses its own bounded disk segments; encoder recovery may produce separate session segments.").variant(TextVariant::Caption))
            .into_any_element()
    }

    pub(super) fn recording_performance_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        Stack::vertical()
            .child(Text::new("Performance"))
            .child(self.button(Action::Diagnostics, "Recording health & counters", None, false, true, cx))
            .child(Text::new("Record 10–20 seconds with moving content and your selected audio sources, then Stop. Verify file integrity and play every track to check sound and synchronization.").variant(TextVariant::Caption))
            .child(self.button(Action::VerifyRecording, "Verify last recording integrity", None, false,
                self.model.snapshot.last_artifact.is_some()
                    && recording_health::verification_allowed(&self.model.snapshot.state, self.model.snapshot.replay_active)
                    && self.model.editable(), cx))
            .into_any_element()
    }

    pub(super) fn recording_observations(&self) -> AnyElement {
        let mut content = Stack::vertical();
        for row in recording_health::diagnostics(self.model.snapshot.metrics.as_ref()) {
            content = content.child(
                Text::new(format!("{}: {}", row.label, row.value)).variant(TextVariant::Caption),
            );
        }
        content.child(Text::new("Clock drift warnings report audio-source clock health. Content A/V offset, duplicate pictures, encoding latency and encoder utilization remain unmeasured when the engine has no observation.").variant(TextVariant::Caption)).into_any_element()
    }

    pub(super) fn verify_recording(&mut self, cx: &mut Context<Self>) {
        if !self.model.editable()
            || !recording_health::verification_allowed(
                &self.model.snapshot.state,
                self.model.snapshot.replay_active,
            )
        {
            return;
        }
        let Some(artifact) = self.model.snapshot.last_artifact.clone() else {
            return;
        };
        self.model.command_pending = true;
        self.model.notice = Some("Verifying finalized session bytes…".into());
        let control = self.services.control.clone();
        let task = cx.background_executor().spawn(async move {
            let published = control.published();
            if !recording_health::verification_allowed(&published.state, published.replay_active) {
                return Err("Stop recording/replay before verifying a file.".into());
            }
            recording_health::verify_finalized(std::path::Path::new(&artifact.path))
        });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                this.model.command_pending = false;
                this.model.notice = Some(match result {
                    Ok(result) => format!("Integrity verified: {} finalized segment(s), {} bytes. Decode, playback and content A/V synchronization still need checking.", result.segments, result.bytes),
                    Err(error) => format!("Recording verification could not pass: {error}"),
                });
                cx.notify();
            });
        }).detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filename_editor_retains_templates_and_blank_restores_automatic() {
        assert_eq!(
            validated_filename(" ").unwrap(),
            frame_runtime::recording_control::AUTO_FILE_STEM
        );
        assert_eq!(
            validated_filename(" client_{date}_{time} ").unwrap(),
            "client_{date}_{time}"
        );
        assert!(validated_filename("{unsupported}").is_err());
        assert!(validated_filename("../outside").is_err());
    }
    #[test]
    fn recording_filename_survives_reload_without_changing_shared_settings_schema() {
        let mut model = AppModel::default();
        let saved = RecordingRequest {
            target: CaptureTarget::Display(DisplayId("test-display".into())),
            fps_mode: FpsMode::AutoMatchSource,
            resolution: Resolution::Native,
            quality: Quality::Standard,
            encoder: EncoderPreference::Auto,
            optimization: None,
            audio_routes: vec![],
            camera: None,
            show_cursor: true,
            output: OutputSettings {
                directory: "C:/recordings".into(),
                file_stem: "Client_{timestamp}".into(),
            },
        };
        // Exercise the existing serialized RecordingRequest, not a parallel settings store.
        let document = serde_json::to_string(&saved).unwrap();
        let restored = serde_json::from_str(&document).unwrap();
        model.initialize(
            model.snapshot.clone(),
            frame_runtime::recording_control::PublishedState::default(),
            Some(restored),
            "C:/fallback".into(),
        );
        assert_eq!(
            model.request.as_ref().unwrap().output.file_stem,
            "Client_{timestamp}"
        );
        assert_eq!(
            model.request.as_ref().unwrap().output.directory,
            "C:/recordings"
        );
    }
}
