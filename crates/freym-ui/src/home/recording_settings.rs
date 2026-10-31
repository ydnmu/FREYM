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
        Stack::vertical()
            .child(
                SettingRow::new(
                    "Filename",
                    Button::new("RecordingFilename", label.to_owned())
                        .size(ButtonSize::Compact)
                        .tooltip(label.to_owned())
                        .disabled(!self.model.editable())
                        .on_click(self.handler(Action::RecordingFilename, cx)),
                )
                .description(
                    "Use {date}, {time} or {timestamp}. Existing files are never overwritten.",
                ),
            )
            .child(SettingRow::new("File format", Text::new("Matroska (.mkv)")))
            .child(SettingRow::new(
                "Interrupted recordings",
                self.button(
                    Action::Recovery,
                    "Recover recordings",
                    None,
                    false,
                    !self.model.recovery_busy,
                    cx,
                ),
            ))
            .child(SettingRow::new(
                "Details",
                self.button(
                    Action::Diagnostics,
                    "Recording details",
                    None,
                    false,
                    true,
                    cx,
                ),
            ))
            .child(
                SettingRow::new(
                    "Last recording",
                    self.button(
                        Action::VerifyRecording,
                        "Verify last recording integrity",
                        None,
                        false,
                        self.model.snapshot.last_artifact.is_some()
                            && recording_health::verification_allowed(
                                &self.model.snapshot.state,
                                self.model.snapshot.replay_active,
                            )
                            && self.model.editable(),
                        cx,
                    ),
                )
                .description(
                    "Checks file integrity. Play the recording to check picture and sound.",
                ),
            )
            .into_any_element()
    }

    pub(super) fn recording_file_details(&self) -> AnyElement {
        Stack::vertical()
            .child(Text::new("Container: Matroska (.mkv). Video: the selected H.264, HEVC or AV1 encoder. Audio: embedded Opus tracks.").variant(TextVariant::Caption))
            .child(Text::new("Recovery creates a separate copy and preserves the original recording and session files.").variant(TextVariant::Caption))
            .into_any_element()
    }

    pub(super) fn recording_performance_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        use crate::{
            ui::components::video_settings::{VideoChoice, VideoDropdown},
            video::VideoAction,
        };
        let p = self.model.preferences.performance;
        let enabled = self.model.editable() && cfg!(target_os = "windows");
        let modes = [
            (PerformanceMode::Maximum, "Maximum Performance"),
            (PerformanceMode::Balanced, "Balanced"),
            (PerformanceMode::Efficiency, "Efficiency"),
        ];
        let cpu = [
            (CpuPriority::Normal, "Normal"),
            (CpuPriority::High, "High · MMCSS"),
        ];
        let mut content=Stack::vertical()
            .child(SettingRow::new("Performance mode",VideoDropdown::new("performance-mode","Performance mode",modes.iter().find(|(v,_)| *v==p.mode).unwrap().1,modes.iter().map(|(mode,label)| VideoChoice::new(*label,VideoAction::Performance(PerformancePolicy{mode:*mode,..p}),p.mode==*mode,true)).collect()).fill_width().disabled(!enabled).on_select(cx.listener(|h,a:&VideoAction,w,cx|h.dispatch(Action::Video(a.clone()),w,cx)))).description("Preview budget: 60 / 30 / 15 FPS. Maximum requests unthrottled media workers; Balanced and Efficiency retain Windows power management."))
            .child(SettingRow::new("Media worker priority",VideoDropdown::new("performance-cpu","Media worker CPU priority",cpu.iter().find(|(v,_)| *v==p.cpu).unwrap().1,cpu.iter().map(|(priority,label)| VideoChoice::new(*label,VideoAction::Performance(PerformancePolicy{cpu:*priority,..p}),p.cpu==*priority,true)).collect()).fill_width().disabled(!enabled).on_select(cx.listener(|h,a:&VideoAction,w,cx|h.dispatch(Action::Video(a.clone()),w,cx)))))
            .child(SettingRow::new("Adaptive performance",Toggle::new("performance-adaptive", "Adaptive performance", p.adaptive).disabled(!enabled).on_click(self.handler(Action::Video(VideoAction::Performance(PerformancePolicy{adaptive:!p.adaptive,..p})),cx))).description("Reduces preview copies under measured drop or queue pressure; restores them after recovery. Recording quality, resolution, FPS and encoder settings are preserved."));
        #[cfg(target_os = "windows")]
        if let Some(error) = frame_windows::performance::failure() {
            content = content.child(Text::new(error).variant(TextVariant::Muted));
        }
        content.into_any_element()
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
            audio_encoding: Default::default(),
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
