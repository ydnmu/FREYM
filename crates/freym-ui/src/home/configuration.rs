use super::*;
use crate::input::TextInput;
use gpui::Focusable;

pub(super) fn shortcut_label(action: ShortcutAction) -> &'static str {
    match action {
        ShortcutAction::StartStop => "Record / stop",
        ShortcutAction::PauseResume => "Pause / resume",
        ShortcutAction::SaveReplay => "Save replay",
    }
}

impl Home {
    pub(super) fn reload_settings(&mut self, cx: &mut Context<Self>) {
        if !self.model.editable() || self.save_running {
            return;
        }
        self.model.connecting = true;
        let services = self.services.clone();
        let task = cx.background_executor().spawn(async move {
            Ok::<_, String>((
                services.store.load_document().map_err(|e| e.to_string())?,
                services.preferences()?,
                services.shortcut_bindings()?,
            ))
        });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                this.model.connecting = false;
                match result {
                    Ok((document, prefs, keys)) => {
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
                        prefs.theme.activate();
                        prefs.surface.activate();
                        crate::ui::theme::set_accent(prefs.accent_color);
                        crate::ui::theme::set_low_cost_effects(prefs.low_cost_effects);
                        crate::ui::theme::set_filled_buttons(prefs.filled_buttons);
                        crate::ui::theme::surface_treatment::set_frosted_transparency(
                            prefs.frosted_transparency,
                        );
                        cx.set_reduce_motion(
                            prefs.reduced_motion || freym_platform::reduced_motion(),
                        );
                        this.model.preferences = prefs;
                        let native = this.native_shortcuts(&keys);
                        if let Some(hotkeys) = &mut this.hotkeys
                            && let Err(error) = hotkeys.configure(&native)
                        {
                            this.shortcut_ui.error = Some(error);
                        }
                        this.save_state.shortcuts = keys.clone();
                        this.model.shortcuts = keys;
                        this.save_state.error = None;
                        this.save_state.startup_dirty = false;
                        this.load_startup(cx);
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
        let task = cx.background_executor().spawn(async move {
            let mut document =
                frame_settings::persistence::SettingsStore::at(services.store.backup_path())
                    .load_document()
                    .map_err(|e| e.to_string())?
                    .ok_or("No previous settings available.")?;
            document.revision = revision;
            Ok::<_, String>(document)
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
                        this.settings_document = Some(document);
                        this.model.settings_blocked = false;
                        this.persist_mode(cx);
                        this.model.error = None;
                        this.model.notice = Some("Previous settings selected.".into());
                        this.check_video_hardware(true, cx);
                    }
                    Err(e) => this.model.error = Some(e.to_string()),
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
            NumberPurpose::Resolution => {
                let d = self.model.source_dimensions().unwrap_or((0, 0));
                vec![("Width", d.0.to_string()), ("Height", d.1.to_string())]
            }
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
        self.number_editor = Some(NumberEditor { purpose, fields });
        self.model.error = None;
    }
    pub(super) fn apply_number(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.number_editor.as_ref().is_some_and(|e| {
            matches!(
                e.purpose,
                NumberPurpose::VideoQp
                    | NumberPurpose::VideoGop
                    | NumberPurpose::Resolution
                    | NumberPurpose::Fps
            )
        }) {
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
                NumberPurpose::VideoQp | NumberPurpose::VideoGop | NumberPurpose::Resolution => {
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
                self.flush_save(cx);
            }
            Err(e) => self.model.error = Some(e),
        }
        cx.notify();
    }
}
