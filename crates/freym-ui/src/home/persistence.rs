use super::*;

#[derive(Default)]
pub(super) struct SaveState {
    pub shortcuts: ShortcutBindings,
    pub startup_dirty: bool,
    pub error: Option<String>,
    pub leave: Option<Action>,
    pub leaving: bool,
    pub exit_confirmed: bool,
}
impl Home {
    pub(super) fn has_unsaved_settings(&self) -> bool {
        self.save_state.error.is_some()
            || self.shortcut_capture_active()
            || self.audio_ui.bitrate_draft.is_some()
            || self.number_editor.is_some()
    }
    pub(super) fn settings_save_status(&self) -> (&str, bool) {
        if self.save_running {
            return ("Saving…", false);
        }
        if self.model.settings_blocked {
            return ("Settings unavailable · reload required", false);
        }
        if let Some(error) = &self.audio_ui.bitrate_draft {
            return (error, false);
        }
        if self.number_editor.is_some() {
            return ("Confirm or cancel the inline value", false);
        }
        if self.camera_ui.invalid_format
            && self
                .model
                .request
                .as_ref()
                .is_some_and(|r| r.camera.is_some())
        {
            return ("Choose an available camera format", false);
        }
        if self.model.command_pending || !self.audio_ui.routes.is_empty() {
            return ("Applying selection…", false);
        }
        if let Some(error) = &self.save_state.error {
            return (error, true);
        }
        if self.save_latest.is_some() || self.settings_document != self.settings_committed {
            return ("Waiting to save", false);
        }
        if self.model.shortcuts != self.save_state.shortcuts || self.save_state.startup_dirty {
            return ("Saving…", false);
        }
        ("Saved", false)
    }
    pub(super) fn clear_save_error(&mut self) {
        if self.save_state.error.is_some() && self.model.error == self.save_state.error {
            self.model.error = None;
        }
        self.save_state.error = None;
    }
    pub(super) fn retry_save(&mut self, cx: &mut Context<Self>) {
        self.clear_save_error();
        self.flush_save(cx);
    }
    pub(super) fn install_settings_document(
        &mut self,
        document: Option<frame_settings::model::SettingsDocument>,
    ) {
        use frame_settings::model::{SettingsDocument, SettingsMode};
        let Some(mut document) =
            document.or_else(|| self.model.request.clone().map(SettingsDocument::new))
        else {
            return;
        };
        let committed = document.clone();
        if let Some(request) = &self.model.request
            && let Ok(mut configuration) = document.configuration()
        {
            configuration.request = request.clone();
            let _ = document.set_configuration(&configuration);
        }
        // Old Audio presentation preference migrates once into schema 3.
        if document.revision == 0
            && self
                .model
                .preferences
                .extra_preferences
                .get("audio_settings")
                .and_then(|v| v.get("advanced"))
                .and_then(|v| v.as_bool())
                == Some(true)
        {
            document.modes.audio = SettingsMode::Advanced;
        }
        self.video
            .switch_mode(if document.modes.video == SettingsMode::Advanced {
                crate::video::VideoMode::Advanced
            } else {
                crate::video::VideoMode::Easy
            });
        self.settings_committed = Some(committed);
        self.settings_document = Some(document);
    }
    /// Accepted settings edits enter the existing revisioned write queue.
    pub(super) fn persist_mode(&mut self, cx: &mut Context<Self>) {
        self.save_latest = Some((
            self.settings_document.clone(),
            self.model.preferences.clone(),
        ));
        self.clear_save_error();
        self.flush_save(cx);
        cx.notify();
    }
    pub(super) fn persist_appearance(&mut self, cx: &mut Context<Self>) {
        let document = self.save_latest.take().and_then(|(d, _)| d);
        self.save_latest = Some((document, self.model.preferences.clone()));
        self.clear_save_error();
        self.flush_save(cx);
        cx.notify();
    }
    pub(super) fn schedule_calibration(&self) {
        #[cfg(target_os = "windows")]
        if let Some(request) = &self.model.request {
            frame_runtime::encoder_calibration::schedule(request.for_capture());
        }
    }
    pub(super) fn refresh(&mut self, cx: &mut Context<Self>) {
        self.model.connecting = true;
        let services = self.services.clone();
        let selected = self.model.request.as_ref().map(|r| r.target.clone());
        let task = cx.background_executor().spawn(async move {
            frame_runtime::discovery::snapshot().map(|mut s| {
                #[cfg(target_os = "windows")]
                if let Some(CaptureTarget::Window(id)) = &selected
                    && !s.windows.iter().any(|w| &w.id == id)
                    && let Ok(window) = frame_windows::validate_window_selection(id)
                {
                    s.windows.push(window);
                }
                #[cfg(not(target_os = "windows"))]
                let _ = selected;
                let p = services.control.observe_sources(s.audio_sources.clone());
                (s, p)
            })
        });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                match result {
                    Ok((s, p)) => {
                        this.model.snapshot = s;
                        this.model.apply_published(p);
                        this.model.reconcile_sources();
                        this.model.error = None;
                    }
                    Err(e) => this.error(e),
                }
                this.model.connecting = false;
                this.check_video_hardware(false, cx);
                this.schedule_calibration();
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn persist(&mut self, cx: &mut Context<Self>) {
        if self.settings_document.is_none() {
            self.install_settings_document(None);
        }
        if let (Some(document), Some(request)) = (&mut self.settings_document, &self.model.request)
        {
            match document.configuration().and_then(|mut config| {
                config.request = request.clone();
                document.set_configuration(&config)
            }) {
                Ok(()) => (),
                Err(error) => {
                    self.save_state.error = Some(error.clone());
                    self.model.error = Some(error);
                    return;
                }
            }
        }
        self.save_latest = Some((
            self.settings_document.clone(),
            self.model.preferences.clone(),
        ));
        self.clear_save_error();
        self.flush_save(cx);
        cx.notify();
    }
    pub(super) fn flush_save(&mut self, cx: &mut Context<Self>) {
        if self.save_running
            || self.save_state.error.is_some()
            || self.model.settings_blocked
            || !self.preferences_loaded
            || self.shortcut_capture_active()
            || self.shortcut_ui.pending.is_some()
            || self.audio_ui.bitrate_draft.is_some()
            || self.number_editor.is_some()
        {
            return;
        }
        // Request changes wait for an idle recorder and acknowledged audio routes.
        // Appearance-only writes remain safe while a recording is active.
        if self
            .save_latest
            .as_ref()
            .is_some_and(|(document, _)| document.is_some())
            && (self.model.connecting
                || self.model.command_pending
                || !self.audio_ui.routes.is_empty()
                || !matches!(
                    self.model.snapshot.state,
                    RecorderState::Idle | RecorderState::Failed(_)
                ))
        {
            return;
        }
        let Some((document, prefs)) = self.save_latest.take() else {
            return;
        };
        let keys = self.model.shortcuts.clone();
        let old_keys = self.save_state.shortcuts.clone();
        let native = self.native_shortcuts(&keys);
        if let Some(hotkeys) = &mut self.hotkeys
            && let Err(error) = hotkeys.configure(&native)
        {
            self.shortcut_ui.error = Some(error.clone());
            self.save_state.error = Some(error);
            self.save_latest = Some((document, prefs));
            cx.notify();
            return;
        }

        self.save_running = true;
        let services = self.services.clone();
        let base = self.settings_committed.clone();
        let previous = document.clone();
        let submitted_prefs = prefs.clone();
        let snapshot = self.model.snapshot.clone();
        let startup = self
            .startup
            .enabled
            .filter(|_| self.save_state.startup_dirty);
        let saved_keys = keys.clone();
        let task = cx.background_executor().spawn(async move {
            let saved = services.save_revisioned(base, document, &prefs, snapshot)?;
            services.save_shortcuts(&keys)?;
            if let Some(enabled) = startup {
                freym_platform::startup::set_enabled(enabled)?;
            }
            Ok::<_, String>(saved)
        });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                this.save_running = false;
                match result {
                    Ok(saved) => {
                        if let Some(document) = saved {
                            if let Some((Some(queued), _)) = &mut this.save_latest {
                                match frame_settings::model::SettingsDocument::rebase_edit(previous.as_ref().unwrap_or(&document), queued, &document) {
                                    Ok(next) => *queued = next,
                                    Err(error) => this.save_state.error = Some(error),
                                }
                            }
                            this.settings_committed = Some(document.clone());
                            if this
                                .save_latest
                                .as_ref()
                                .is_none_or(|(queued, _)| queued.is_none())
                            {
                                if let Ok(configuration)=document.configuration() {
                                    if this.model.editable() {
                                        this.services.control.restore_routes(&configuration.request.audio_routes);
                                        let mut sources=this.model.snapshot.audio_sources.clone();
                                        audio::retain_saved_sources(&mut sources,&configuration.request.audio_routes);
                                        this.model.apply_published(this.services.control.observe_sources(sources));
                                    }
                                    this.model.request=Some(configuration.request);
                                }
                                this.install_settings_document(Some(document));
                            } else if let Some((Some(queued),_))=&this.save_latest {
                                if let Ok(configuration)=queued.configuration() {this.model.request=Some(configuration.request);}
                                this.settings_document=Some(queued.clone());
                            }
                        }
                        this.save_state.shortcuts = saved_keys;
                        if startup == this.startup.enabled { this.save_state.startup_dirty = false; }
                        if let Some(updater) = &this.updates { updater.set_enabled(submitted_prefs.automatic_updates); }
                        if this.save_latest.is_none() {
                            this.clear_save_error();
                        }
                        this.shortcut_ui.error = None;
                        this.flush_save(cx);
                    }
                    Err(error) => {
                        if saved_keys != old_keys {
                            let native = this.native_shortcuts(&old_keys);
                            if let Some(hotkeys) = &mut this.hotkeys
                                && let Err(rollback) = hotkeys.configure(&native) { this.shortcut_ui.error = Some(rollback); }
                        }
                        // Storage can have committed before a later OS preference
                        // failed. Keep the draft and expose the partial failure;
                        // retry uses the store's existing revision/conflict rebase.
                        match &mut this.save_latest {
                            Some((queued, _)) if queued.is_none() => *queued = previous,
                            None => this.save_latest = Some((previous, submitted_prefs)),
                            _ => (),
                        }
                        this.save_state.error = Some(format!("Could not save settings: {error}. Some settings may already be written. Retry or reload."));
                    }
                }
                if let Some(error) = &this.save_state.error {
                    this.model.error = Some(error.clone());
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }
    pub(super) fn choose_output(&mut self, cx: &mut Context<Self>) {
        let answer = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Recording folder".into()),
        });
        cx.spawn(async move |view, cx| match answer.await {
            Ok(Ok(Some(paths))) => {
                let _ = view.update(cx, |this, cx| {
                    if this.model.editable() {
                        if let (Some(r), Some(path)) = (&mut this.model.request, paths.first()) {
                            r.output.directory = path.to_string_lossy().into_owned();
                        }
                        this.persist(cx);
                        this.scan_recovery(cx);
                        cx.notify();
                    }
                });
            }
            Ok(Ok(None)) => {}
            other => {
                let _ = view.update(cx, |this, cx| {
                    this.model.error = Some(format!("Output folder selection failed: {other:?}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }
    pub(super) fn scan_recovery(&mut self, cx: &mut Context<Self>) {
        let Some(r) = &self.model.request else {
            return;
        };
        let directory = r.output.directory.clone();
        let expected = directory.clone();
        let task = cx.background_executor().spawn(async move {
            frame_runtime::recovery::discover_available(std::path::Path::new(&directory)).map(
                |scan| {
                    let metadata = scan
                        .paths
                        .iter()
                        .map(|path| {
                            (
                                path.clone(),
                                frame_runtime::recovery::file_description(std::path::Path::new(
                                    path,
                                )),
                            )
                        })
                        .collect();
                    (scan, metadata)
                },
            )
        });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                if this
                    .model
                    .request
                    .as_ref()
                    .is_none_or(|r| r.output.directory != expected)
                {
                    return;
                }
                match result {
                    Ok((scan, metadata)) => {
                        this.model.interrupted = scan.paths;
                        this.model.recovery_metadata = metadata;
                        if this
                            .model
                            .recovery_selected
                            .as_ref()
                            .is_some_and(|path| !this.model.interrupted.contains(path))
                        {
                            this.model.recovery_selected = None;
                        }
                        if scan.truncated {
                            this.model.notice = Some(
                                "More interrupted files may exist. Choose a file to recover."
                                    .into(),
                            );
                        }
                    }
                    Err(e) => this.error(e),
                }
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn recover(&mut self, source: Option<String>, cx: &mut Context<Self>) {
        if self.model.recovery_busy
            || !matches!(
                self.model.snapshot.state,
                RecorderState::Idle | RecorderState::Failed(_)
            )
        {
            return;
        }
        let answer = if source.is_none() {
            Some(cx.prompt_for_paths(PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: Some("Choose interrupted MKV or FREYM session manifest".into()),
            }))
        } else {
            None
        };
        let directory = self
            .model
            .request
            .as_ref()
            .map(|r| r.output.directory.clone())
            .unwrap_or_default();
        self.model.recovery_busy = true;
        cx.spawn(async move |view, cx| {
            let source = match (source, answer) {
                (Some(p), _) => Some(std::path::PathBuf::from(p)),
                (_, Some(answer)) => match answer.await {
                    Ok(Ok(Some(paths))) => paths.first().cloned(),
                    Ok(Ok(None)) => None,
                    other => {
                        let _ = view.update(cx, |this, cx| {
                            this.model.error =
                                Some(format!("Recovery source selection failed: {other:?}"));
                            cx.notify();
                        });
                        None
                    }
                },
                _ => None,
            };
            let Some(source) = source else {
                let _ = view.update(cx, |this, cx| {
                    this.model.recovery_busy = false;
                    cx.notify();
                });
                return;
            };
            let name = format!(
                "{}-recovered.{}",
                source.file_stem().unwrap_or_default().to_string_lossy(),
                if source.extension().is_some_and(|e| e == "freym-session") {
                    "freym-session"
                } else {
                    "mkv"
                }
            );
            let answer = cx
                .update(|cx| cx.prompt_for_new_path(std::path::Path::new(&directory), Some(&name)));
            let output = match answer.await {
                Ok(Ok(p)) => p,
                other => {
                    let _ = view.update(cx, |this, cx| {
                        this.model.error =
                            Some(format!("Recovery destination selection failed: {other:?}"));
                        cx.notify();
                    });
                    None
                }
            };
            let Some(output) = output else {
                let _ = view.update(cx, |this, cx| {
                    this.model.recovery_busy = false;
                    cx.notify();
                });
                return;
            };
            let Some(services) = view.update(cx, |this, _| this.services.clone()).ok() else {
                return;
            };
            let task = cx.background_executor().spawn(async move {
                services
                    .recovery
                    .begin(&services.control.published().state)?
                    .run_any(&source, &output)
            });
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                this.model.recovery_busy = false;
                match result {
                    Ok(result) => this.model.recovery_result = Some(result),
                    Err(e) => this.error(e),
                }
                this.scan_recovery(cx);
                cx.notify();
            });
        })
        .detach();
    }
}
