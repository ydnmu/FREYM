use super::*;
use crate::video::{
    VideoAction,
    adapter::{self, VideoContext},
};

impl Home {
    pub(super) fn video_native_size(&self) -> Option<Dimensions> {
        let context = self.video_context()?;
        context.dimensions.or_else(|| {
            self.video
                .capabilities_for(&context)
                .map(|c| c.native_dimensions)
        })
    }
    pub(super) fn validate_video_rate(&self, fps: f64) -> Result<(), String> {
        if !self.model.snapshot.capabilities.fixed_fps {
            return Err("Fixed FPS is unavailable for this source.".into());
        }
        frame_settings::policy::NominalRate::recover_exact(fps).map_err(|e| e.message)?;
        if let Some(native) = self.model.source_rate_hz()
            && fps > native + 0.001
        {
            return Err("Recording FPS cannot exceed the observed source refresh rate.".into());
        }
        let context = self.video_context().ok_or("Choose a capture source.")?;
        let caps = self
            .video
            .capabilities_for(&context)
            .ok_or("Hardware qualification pending.")?;
        let mut next = self
            .model
            .request
            .clone()
            .ok_or("Choose a capture source.")?;
        next.fps_mode = FpsMode::Fixed(fps);
        caps.plan(&next).map(|_| ())
    }
    /// UI admission guard. Core must enforce the same rule for non-UI starts.
    pub(super) fn video_start_error(&self) -> Option<String> {
        if !cfg!(target_os = "windows") {
            return None;
        }

        let context = self.video_context()?;
        let request = self.model.request.as_ref()?;
        let caps = self.video.capabilities_for(&context);
        if let Some(caps) = caps {
            return caps.plan(request).err();
        }
        let manual = matches!(request.encoder, EncoderPreference::Explicit(_))
            || request.resolution != Resolution::Native
            || request.optimization.as_ref().is_some_and(|o| {
                !o.automatic_quality
                    || o.codec != RecordingCodec::Auto
                    || o.bitrate_bps.is_some()
                    || !o.encoder_overrides.is_empty()
                    || matches!(
                        o.preset,
                        RecordingPreset::Custom | RecordingPreset::Lossless
                    )
            })
            || (request.optimization.is_none() && request.quality != Quality::Standard);
        manual.then(|| "Check Video hardware for the current source and frame rate before recording with manual encoder settings.".into())
    }
    pub(super) fn apply_video_number(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.model.editable() || self.model.settings_blocked {
            return;
        }
        let Some(editor) = &self.number_editor else {
            return;
        };
        let value = editor.fields[0].1.read(cx).value().trim().to_owned();
        let is_fps = matches!(editor.purpose, NumberPurpose::Fps);
        let native_number = match editor.purpose {
            NumberPurpose::VideoQp => Some(true),
            NumberPurpose::VideoGop => Some(false),
            _ => None,
        };
        let Some(request) = &self.model.request else {
            return;
        };
        let mut next = request.clone();
        let result: Result<(), String> = (|| {
            if let Some(is_qp) = native_number {
                let options = next
                    .optimization
                    .get_or_insert_with(|| RecordingOptimization {
                        preset: RecordingPreset::Custom,
                        automatic_quality: false,
                        ..Default::default()
                    });
                if is_qp {
                    if options.encoder_overrides.rate_control != Some(VideoRateControl::Cqp) {
                        return Err("Select CQP before editing QP.".into());
                    }
                    options.encoder_overrides.qp = Some(
                        value
                            .parse::<u32>()
                            .ok()
                            .filter(|n| *n <= 51)
                            .ok_or("Enter a whole QP in 0..51.")?,
                    );
                } else {
                    options.encoder_overrides.gop_frames = if value.is_empty() {
                        None
                    } else {
                        Some(value.parse::<u32>().ok().filter(|n| *n > 0 && *n < u32::MAX).ok_or("Enter a finite positive frame interval, or leave empty for automatic.")?)
                    };
                }
                let context = self.video_context().ok_or("Choose a capture source.")?;
                let caps = self
                    .video
                    .capabilities_for(&context)
                    .ok_or("Check hardware before editing native settings.")?;
                if !caps
                    .eligible(request)
                    .any(|e| e.backend == adapter::EncoderBackend::Nvenc)
                {
                    return Err("Native numeric controls require a qualified NVENC encoder.".into());
                }
                caps.plan(&next)?;
            } else if matches!(editor.purpose, NumberPurpose::Resolution) {
                let dimension = |i: usize| {
                    editor.fields[i]
                        .1
                        .read(cx)
                        .value()
                        .trim()
                        .parse::<u32>()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or("Enter positive whole pixel dimensions.")
                };
                let size = Dimensions {
                    width: dimension(0)?,
                    height: dimension(1)?,
                };
                let context = self.video_context().ok_or("Choose a capture source.")?;
                next = adapter::apply_edit(
                    request,
                    &context,
                    self.video.capabilities_for(&context),
                    &crate::video::VideoEdit::Resolution(size),
                )?;
            } else if is_fps {
                if !self.model.snapshot.capabilities.fixed_fps {
                    return Err("Fixed frame rate is unavailable for this capture path.".into());
                }
                let fps = value
                    .parse::<f64>()
                    .map_err(|_| "Enter a finite, positive frame rate.")?;
                frame_settings::policy::NominalRate::recover_exact(fps).map_err(|e| e.message)?;
                next.fps_mode = FpsMode::Fixed(fps);
                self.validate_video_rate(fps)?;
            } else {
                let bitrate = if value.is_empty() {
                    None
                } else {
                    Some(
                        value
                            .parse::<u32>()
                            .ok()
                            .filter(|b| *b > 0)
                            .ok_or("Enter positive bit/s, or leave empty for automatic.")?,
                    )
                };
                next.optimization
                    .get_or_insert_with(|| RecordingOptimization {
                        preset: RecordingPreset::Custom,
                        automatic_quality: false,
                        ..Default::default()
                    })
                    .bitrate_bps = bitrate;
                let context = self.video_context().ok_or("Choose a capture source.")?;
                self.video
                    .capabilities_for(&context)
                    .ok_or("Check hardware before setting a bitrate.")?
                    .plan(&next)?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.model.request = Some(next);
                self.model.error = None;
                self.number_editor = None;
                window.focus(&self.focus, cx);
                self.persist(cx);
                self.check_video_hardware(false, cx);
            }
            Err(error) => self.model.error = Some(error),
        }
        cx.notify();
    }
    pub(super) fn video_context(&self) -> Option<VideoContext> {
        VideoContext::from_request(self.model.request.as_ref()?, &self.model.snapshot)
    }
    pub(super) fn check_video_hardware(&mut self, force: bool, cx: &mut Context<Self>) {
        // The native workbench has no discovery or hardware side effects.

        if !self.model.editable() || !self.model.snapshot.capabilities.can_record {
            return;
        }
        let Some(context) = self.video_context() else {
            return;
        };
        if self.video.probing.as_ref() == Some(&context)
            || (!force && self.video.capabilities_for(&context).is_some())
        {
            return;
        }
        let generation = self.video.begin_probe(context.clone());
        let task = cx
            .background_executor()
            .spawn(async move { adapter::discover(context) });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                this.video.finish_probe(generation, result);
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn dispatch_video(
        &mut self,
        action: VideoAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            VideoAction::Audio(event) => self.dispatch(Action::AudioSettings(event), window, cx),
            VideoAction::Performance(policy) if self.model.editable() => {
                let preview = false;
                let result = if preview {
                    Ok(())
                } else {
                    self.services.control.configure_performance(policy)
                };
                match result {
                    Ok(()) => {
                        self.model.preferences.performance = policy;
                        self.persist_appearance(cx);
                    }
                    Err(e) => self.error(e),
                }
            }
            VideoAction::CameraDevice(id) => self.dispatch(Action::SelectCamera(id), window, cx),
            VideoAction::CameraFormat(index) => {
                self.dispatch(Action::SelectCameraFormat(index), window, cx)
            }
            VideoAction::CameraToggle => self.toggle_camera(cx),
            VideoAction::ResolutionEditor if self.model.editable() => {
                self.edit_number(NumberPurpose::Resolution, window, cx)
            }
            VideoAction::Mode(mode) if !self.model.closing => {
                if self.settings_document.is_none() {
                    self.install_settings_document(None);
                }
                self.video.switch_mode(mode);
                if let Some(document) = &mut self.settings_document {
                    document.modes.video = if mode == crate::video::VideoMode::Advanced {
                        frame_settings::model::SettingsMode::Advanced
                    } else {
                        frame_settings::model::SettingsMode::Easy
                    };
                }
                self.persist_mode(cx);
            }
            VideoAction::Expand(section) if !self.model.closing => {
                self.video.expanded[section.index()] = !self.video.expanded[section.index()];
            }
            VideoAction::Refresh => self.check_video_hardware(true, cx),
            VideoAction::Edit(edit) if self.model.editable() && !self.model.settings_blocked => {
                if let crate::video::VideoEdit::Fps(Some(millihz)) = &edit
                    && let Err(error) = self.validate_video_rate(f64::from(*millihz) / 1000.)
                {
                    self.model.error = Some(error);
                    cx.notify();
                    return;
                }
                let Some(context) = self.video_context() else {
                    return;
                };
                let Some(request) = &self.model.request else {
                    return;
                };
                match adapter::apply_edit(
                    request,
                    &context,
                    self.video.capabilities_for(&context),
                    &edit,
                ) {
                    Ok(next) => {
                        self.model.request = Some(next);
                        self.model.error = None;
                        self.persist(cx);
                        self.check_video_hardware(false, cx);
                    }
                    Err(error) => self.model.error = Some(error),
                }
            }
            VideoAction::FpsEditor
                if self.model.editable() && self.model.snapshot.capabilities.fixed_fps =>
            {
                self.edit_number(NumberPurpose::Fps, window, cx);
            }
            VideoAction::BitrateEditor if self.model.editable() => {
                self.edit_number(NumberPurpose::Bitrate, window, cx)
            }
            VideoAction::QpEditor if self.model.editable() => {
                self.edit_number(NumberPurpose::VideoQp, window, cx)
            }
            VideoAction::GopEditor if self.model.editable() => {
                self.edit_number(NumberPurpose::VideoGop, window, cx)
            }
            _ => {}
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests;
