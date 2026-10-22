use super::*;
use crate::input::TextInput;
use gpui::{Focusable, KeyDownEvent};

pub(super) fn shortcut_label(action: ShortcutAction) -> &'static str {
    match action {
        ShortcutAction::StartStop => "Record / stop",
        ShortcutAction::PauseResume => "Pause / resume",
        ShortcutAction::SaveReplay => "Save replay",
    }
}

impl Home {
    pub(super) fn open_preview(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.preview_enabled = true;
        #[cfg(target_os = "windows")]
        if let Some(preview) = &self.preview {
            preview.update(cx, |preview, _| preview.retry());
        }
        cx.notify();
    }
    pub(super) fn reload_settings(&mut self, cx: &mut Context<Self>) {
        if !self.model.editable() || self.save_running {
            return;
        }
        self.model.connecting = true;
        let services = self.services.clone();
        let task = cx
            .background_executor()
            .spawn(async move { services.store.load_document() });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                this.model.connecting = false;
                match result {
                    Ok(document) => {
                        let saved = match document
                            .as_ref()
                            .map(|d| d.configuration().map(|c| c.request))
                            .transpose()
                        {
                            Ok(saved) => saved,
                            Err(e) => {
                                this.model.settings_blocked = true;
                                this.model.error = Some(e);
                                return;
                            }
                        };
                        if let Some(saved) = &saved {
                            this.services.control.restore_routes(&saved.audio_routes);
                        }
                        let snapshot = this.model.snapshot.clone();
                        let p = this
                            .services
                            .control
                            .observe_sources(snapshot.audio_sources.clone());
                        let output = this
                            .model
                            .request
                            .as_ref()
                            .map(|r| r.output.directory.clone())
                            .unwrap_or_default();
                        this.model.initialize(snapshot, p, saved, output);
                        this.save_latest = None;
                        this.install_settings_document(document);
                        this.model.settings_blocked = false;
                        this.model.error = None;
                    }
                    Err(e) => {
                        this.model.settings_blocked = true;
                        this.model.error = Some(e.to_string());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn rollback_settings(&mut self, cx: &mut Context<Self>) {
        if !self.model.editable() || self.save_running || self.save_latest.is_some() {
            return;
        }
        let Some(revision) = self.settings_committed.as_ref().map(|d| d.revision) else {
            return;
        };
        self.model.connecting = true;
        let services = self.services.clone();
        let state = self.model.snapshot.state.clone();
        let task = cx.background_executor().spawn(async move {
            services.store.rollback(
                revision,
                &state,
                frame_settings::validation::ApplyTiming::Now,
            )
        });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                this.model.connecting = false;
                match result {
                    Ok(document) => {
                        if let Ok(config) = document.configuration() {
                            this.services
                                .control
                                .restore_routes(&config.request.audio_routes);
                            let p = this
                                .services
                                .control
                                .observe_sources(this.model.snapshot.audio_sources.clone());
                            this.model.apply_published(p);
                            this.model.request = Some(config.request);
                        }
                        this.install_settings_document(Some(document));
                        this.model.settings_blocked = false;
                        this.model.error = None;
                        this.model.notice =
                            Some("Previous settings restored. Rollback remains reversible.".into());
                        this.check_video_hardware(true, cx);
                    }
                    Err(e) => this.model.error = Some(e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn capture_shortcut(
        &mut self,
        event: &KeyDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(action) = self.shortcut_edit else {
            return;
        };
        cx.stop_propagation();
        if event.is_held {
            return;
        }
        let key = &event.keystroke.key;
        if key == "escape" {
            self.shortcut_edit = None;
            cx.notify();
            return;
        }
        let mut parts = Vec::new();
        let m = event.keystroke.modifiers;
        if m.control {
            parts.push("Ctrl".into());
        }
        if m.alt {
            parts.push("Alt".into());
        }
        if m.shift {
            parts.push("Shift".into());
        }
        if m.platform {
            parts.push("Super".into());
        }
        parts.push(key.to_uppercase());
        let text = if key == "backspace" {
            String::new()
        } else {
            parts.join("+")
        };
        let mut next = self.model.shortcuts.clone();
        match action {
            ShortcutAction::StartStop => next.start_stop = text,
            ShortcutAction::PauseResume => next.pause_resume = text,
            ShortcutAction::SaveReplay => next.save_replay = text,
        }
        self.shortcut_edit = None;
        self.configure_shortcuts(next, cx);
        cx.notify();
    }
    fn configure_shortcuts(&mut self, next: ShortcutBindings, cx: &mut Context<Self>) {
        #[cfg(feature = "ui-preview")]
        if self.ui_preview {
            self.model.shortcuts = next;
            return;
        }
        let Some(hotkeys) = &mut self.hotkeys else {
            return;
        };
        if let Err(e) = hotkeys.configure(&next) {
            self.model.error = Some(e);
            return;
        }
        self.model.shortcuts_pending = true;
        let previous = self.model.shortcuts.clone();
        let services = self.services.clone();
        let task = cx
            .background_executor()
            .spawn(async move { services.save_shortcuts(&next).map(|()| next) });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                this.model.shortcuts_pending = false;
                match result {
                    Ok(next) => this.model.shortcuts = next,
                    Err(e) => {
                        let rollback = this.hotkeys.as_mut().map(|h| h.configure(&previous));
                        this.model.error = Some(format!(
                            "Could not save shortcuts: {e}. {}",
                            match rollback {
                                Some(Err(e)) => e,
                                _ => "Previous bindings restored.".into(),
                            }
                        ));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn edit_number(
        &mut self,
        purpose: NumberPurpose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(r) = &self.model.request else {
            return;
        };
        let values: Vec<(&'static str, String)> = match purpose {
            NumberPurpose::VideoQp => vec![(
                "Constant QP (whole number 0..51)",
                r.optimization
                    .as_ref()
                    .and_then(|o| o.encoder_overrides.qp)
                    .map(|n| n.to_string())
                    .unwrap_or_default(),
            )],
            NumberPurpose::VideoGop => vec![(
                "Keyframe interval (accepted frames, empty = automatic)",
                r.optimization
                    .as_ref()
                    .and_then(|o| o.encoder_overrides.gop_frames)
                    .map(|n| n.to_string())
                    .unwrap_or_default(),
            )],
            NumberPurpose::RecordingFilename => vec![(
                "Filename or template, without .mkv (empty = automatic)",
                if r.output.file_stem == frame_runtime::recording_control::AUTO_FILE_STEM {
                    String::new()
                } else {
                    r.output.file_stem.clone()
                },
            )],
            NumberPurpose::Fps => vec![(
                "Requested frames per second",
                match r.fps_mode {
                    FpsMode::Fixed(fps) => fps.to_string(),
                    FpsMode::AutoMatchSource => self
                        .model
                        .source_rate_hz()
                        .map(|rate| rate.to_string())
                        .unwrap_or_default(),
                },
            )],
            NumberPurpose::Bitrate => vec![(
                "Bitrate (bit/s, empty = automatic)",
                r.optimization
                    .as_ref()
                    .and_then(|o| o.bitrate_bps)
                    .map(|b| b.to_string())
                    .unwrap_or_default(),
            )],
            NumberPurpose::CameraPlacement => match &r.camera {
                Some(c) => vec![
                    ("Left (pixels)", c.placement.x.to_string()),
                    ("Top (pixels)", c.placement.y.to_string()),
                    ("Width (pixels)", c.placement.width.to_string()),
                    ("Height (pixels)", c.placement.height.to_string()),
                ],
                None => return,
            },
            NumberPurpose::ReplayMemory => vec![
                (
                    "Maximum replay memory (MiB)",
                    (self.model.snapshot.replay_settings.max_bytes / 1024 / 1024).to_string(),
                ),
                (
                    "Maximum replay storage (MiB)",
                    self.model.snapshot.replay_settings.storage_mib.to_string(),
                ),
            ],
        };
        let fields = values
            .into_iter()
            .map(|(label, value)| (label, cx.new(|cx| TextInput::new(value, "", cx))))
            .collect::<Vec<_>>();
        window.focus(&fields[0].1.focus_handle(cx), cx);
        let slider = if matches!(purpose, NumberPurpose::Fps)
            && crate::ui::theme::ThemeId::current().is_motion()
            && self.model.editable()
            && self.model.snapshot.capabilities.fixed_fps
        {
            let input = fields[0].1.clone();
            let max = input
                .read(cx)
                .value()
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .unwrap_or(240.)
                .max(240.);
            Some(cx.new(|cx| {
                crate::ui::components::motion_slider::MotionSlider::new(input, 1., max, cx)
            }))
        } else {
            None
        };
        self.number_editor = Some(NumberEditor {
            purpose,
            fields,
            slider,
        });
        self.model.error = None;
    }
    pub(super) fn apply_number(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .number_editor
            .as_ref()
            .is_some_and(|e| matches!(e.purpose, NumberPurpose::VideoQp | NumberPurpose::VideoGop))
        {
            self.apply_video_number(window, cx);
            return;
        }
        if !self.model.editable() {
            return;
        }
        let Some(editor) = &self.number_editor else {
            return;
        };
        let values = editor
            .fields
            .iter()
            .map(|(_, input)| input.read(cx).value().trim().to_owned())
            .collect::<Vec<_>>();
        let purpose = editor.purpose;
        let result: Result<(), String> = (|| {
            match purpose {
                NumberPurpose::VideoQp | NumberPurpose::VideoGop => {
                    unreachable!("Video editors use native admission")
                }
                NumberPurpose::RecordingFilename => {
                    let stem = crate::home::recording_settings::validated_filename(&values[0])?;
                    if let Some(r) = &mut self.model.request {
                        r.output.file_stem = stem;
                    }
                    self.persist(cx);
                }
                NumberPurpose::Fps => {
                    let fps = values[0]
                        .parse::<f64>()
                        .map_err(|_| "Enter a positive frame rate.")?;
                    if !fps.is_finite() || fps <= 0. {
                        return Err("Enter a finite, positive frame rate.".into());
                    }
                    if let Some(r) = &mut self.model.request {
                        r.fps_mode = FpsMode::Fixed(fps);
                    }
                    self.persist(cx);
                }
                NumberPurpose::Bitrate => {
                    let bitrate =
                        if values[0].is_empty() {
                            None
                        } else {
                            Some(
                                values[0].parse::<u32>().ok().filter(|b| *b > 0).ok_or(
                                    "Enter a positive bitrate, or leave empty for automatic.",
                                )?,
                            )
                        };
                    if let Some(r) = &mut self.model.request {
                        r.optimization
                            .get_or_insert_with(|| RecordingOptimization {
                                automatic_quality: false,
                                ..RecordingOptimization::default()
                            })
                            .bitrate_bps = bitrate;
                    }
                    self.persist(cx);
                }
                NumberPurpose::CameraPlacement => {
                    let numbers = values
                        .iter()
                        .map(|v| {
                            v.parse::<u32>()
                                .map_err(|_| "Enter whole, nonnegative pixel values.".to_owned())
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    let (x, y, width, height) = (numbers[0], numbers[1], numbers[2], numbers[3]);
                    let Some((source_w, source_h)) = self.model.source_dimensions() else {
                        return Err("Choose a screen or area first.".into());
                    };
                    if width == 0
                        || height == 0
                        || x.checked_add(width).is_none_or(|v| v > source_w)
                        || y.checked_add(height).is_none_or(|v| v > source_h)
                    {
                        return Err(
                            "The camera must fit within the selected screen or area.".into()
                        );
                    }
                    if let Some(c) = self.model.request.as_mut().and_then(|r| r.camera.as_mut()) {
                        c.placement = CameraPlacement {
                            x,
                            y,
                            width,
                            height,
                        };
                    }
                    self.persist(cx);
                }
                NumberPurpose::ReplayMemory => {
                    let mib = values[0]
                        .parse::<u32>()
                        .map_err(|_| "Enter a whole number of MiB.")?;
                    let max_bytes = mib
                        .checked_mul(1024 * 1024)
                        .filter(|v| (1024 * 1024..=512 * 1024 * 1024).contains(v))
                        .ok_or("Enter between 1 and 512 MiB of replay memory.")?;
                    let storage_mib = values[1]
                        .parse::<u32>()
                        .map_err(|_| "Enter a whole number of MiB.")?;
                    if !(16..=16384).contains(&storage_mib) {
                        return Err("Enter between 16 and 16384 MiB of replay storage.".into());
                    }
                    let settings = ReplaySettings {
                        max_bytes,
                        storage_mib,
                        ..self.model.snapshot.replay_settings
                    };
                    #[cfg(feature = "ui-preview")]
                    if self.ui_preview {
                        self.model.snapshot.replay_settings = settings;
                        return Ok(());
                    }
                    let services = self.services.clone();
                    let task = cx
                        .background_executor()
                        .spawn(async move { services.control.configure_replay(settings) });
                    cx.spawn(async move |view, cx| {
                        let result = task.await;
                        let _ = view.update(cx, |this, cx| {
                            if let Err(e) = result {
                                this.error(e);
                            }
                            cx.notify();
                        });
                    })
                    .detach();
                }
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.number_editor = None;
                window.focus(&self.focus, cx);
            }
            Err(e) => self.model.error = Some(e),
        }
        cx.notify();
    }
}
