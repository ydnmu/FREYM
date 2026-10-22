use super::*;

impl Home {
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
        self.settings_committed = Some(document.clone());
        self.settings_document = Some(document);
    }
    pub(super) fn persist_mode(&mut self, cx: &mut Context<Self>) {
        #[cfg(feature = "ui-preview")]
        if self.ui_preview {
            return;
        }
        if self.model.settings_blocked {
            return;
        }
        self.save_latest = Some((
            self.settings_document.clone(),
            self.model.preferences.clone(),
        ));
        self.flush_save(cx);
    }
    /// Theme-only edits do not schedule encoder calibration or rewrite requests.
    pub(super) fn persist_appearance(&mut self, cx: &mut Context<Self>) {
        #[cfg(feature = "ui-preview")]
        if self.ui_preview {
            return;
        }
        // Preserve an already queued recording-settings write when coalescing.
        let request = self.save_latest.take().and_then(|(request, _)| request);
        self.save_latest = Some((request, self.model.preferences.clone()));
        self.flush_save(cx);
    }
    pub(super) fn schedule_calibration(&self) {
        #[cfg(feature = "ui-preview")]
        if self.ui_preview {
            return;
        }
        #[cfg(target_os = "windows")]
        if let Some(request) = &self.model.request {
            frame_runtime::encoder_calibration::schedule(request.clone());
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
                        this.model.error = None;
                    }
                    Err(e) => this.error(e),
                }
                this.model.connecting = false;
                this.schedule_calibration();
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn persist(&mut self, cx: &mut Context<Self>) {
        #[cfg(feature = "ui-preview")]
        if self.ui_preview {
            return;
        }
        self.schedule_calibration();
        let document = if self.model.settings_blocked {
            None
        } else {
            if self.settings_document.is_none() {
                self.install_settings_document(None);
            }
            if let (Some(document), Some(request)) =
                (&mut self.settings_document, &self.model.request)
                && let Ok(mut config) = document.configuration()
            {
                config.request = request.clone();
                if let Err(e) = document.set_configuration(&config) {
                    self.model.error = Some(e);
                    return;
                }
            }
            self.settings_document.clone()
        };
        self.save_latest = Some((document, self.model.preferences.clone()));
        self.flush_save(cx);
    }
    pub(super) fn flush_save(&mut self, cx: &mut Context<Self>) {
        if self.save_running {
            return;
        }
        // Live mute reaches the dedicated command now; persist the settled routes
        // after Stop, without probing encoders during an active session.
        if self.save_latest.as_ref().is_some_and(|(d, _)| d.is_some())
            && (self.model.command_pending
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
        self.save_running = true;
        let services = self.services.clone();
        let base = self.settings_committed.clone();
        // Queued edits were made on the submitted draft. Diff against that draft
        // so reverting an in-flight edit is retained as an explicit new intent.
        let previous = document.clone();
        let snapshot = self.model.snapshot.clone();
        let task = cx
            .background_executor()
            .spawn(async move { services.save_revisioned(base, document, &prefs, snapshot) });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                this.save_running = false;
                match result {
                    Ok(Some(document)) => {
                        if let Some((Some(queued), _)) = &mut this.save_latest {
                            match frame_settings::model::SettingsDocument::rebase_edit(
                                previous.as_ref().unwrap_or(&document), queued, &document,
                            ) {
                                Ok(next) => *queued = next,
                                Err(e) => {
                                    this.save_latest = None;
                                    this.model.settings_blocked = true;
                                    this.model.error = Some(e);
                                }
                            }
                        }
                        this.settings_committed = Some(document.clone());
                        if this.save_latest.as_ref().is_none_or(|(d, _)| d.is_none()) {
                            if let Ok(c) = document.configuration() {
                                if this.model.editable() {
                                    this.services.control.restore_routes(&c.request.audio_routes);
                                    let mut sources = this.model.snapshot.audio_sources.clone();
                                    audio::retain_saved_sources(&mut sources, &c.request.audio_routes);
                                    let p = this.services.control.observe_sources(sources);
                                    this.model.apply_published(p);
                                }
                                this.model.request = Some(c.request);
                            }
                            this.install_settings_document(Some(document));
                        } else if let Some((Some(queued), _)) = &this.save_latest {
                            if let Ok(c) = queued.configuration() { this.model.request = Some(c.request); }
                            this.settings_document = Some(queued.clone());
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        this.save_latest = None;
                        this.model.settings_blocked = true;
                        this.model.error = Some(format!("Could not finish saving settings: {e}. Reload settings before recording or reapplying edits."));
                    }
                }
                cx.notify();
                this.flush_save(cx);
            });
        })
        .detach();
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
            frame_runtime::recovery::discover_available(std::path::Path::new(&directory))
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
                    Ok(scan) => {
                        this.model.interrupted = scan.paths;
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
