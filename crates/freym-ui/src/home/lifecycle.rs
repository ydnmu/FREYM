use super::*;

impl Home {
    pub(super) fn error(&mut self, e: RecorderError) {
        self.model.error = Some(crate::services::error_details(&e));
    }
    pub(super) fn record(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.snapshot.state == RecorderState::Idle && self.model.staged_target.is_some() {
            self.apply_staged_target(window, cx);
            return;
        }
        if self.model.command_pending || self.model.closing || self.model.recovery_busy {
            return;
        }
        let state = self.model.snapshot.state.clone();
        if state == RecorderState::Idle && self.save_running {
            self.model.notice = Some("Wait for settings to finish saving before recording.".into());
            cx.notify();
            return;
        }
        let request = if state == RecorderState::Idle {
            if self.camera_ui.invalid_format
                && self
                    .model
                    .request
                    .as_ref()
                    .is_some_and(|r| r.camera.is_some())
            {
                self.model.error=Some("Refresh the camera and choose an available capture capability before recording.".into());
                cx.notify();
                return;
            }
            if !self.model.can_start() {
                return;
            }
            if let Some(error) = self.video_start_error() {
                self.model.error = Some(error);
                cx.notify();
                return;
            }
            self.model.sync_routes();
            if let Err(error) = self.validate_audio_recording() {
                self.error(error);
                cx.notify();
                return;
            }
            self.model.request.clone()
        } else {
            None
        };
        if state == RecorderState::Finalizing {
            return;
        }
        self.model.command_pending = true;
        self.model.error = None;
        let configuration = self
            .settings_document
            .as_ref()
            .map(|d| {
                let mut config = d.configuration()?;
                if let Some(request) = &request {
                    config.request = request.clone();
                }
                Ok::<_, String>(config)
            })
            .transpose();
        let configuration = match configuration {
            Ok(c) => c,
            Err(e) => {
                self.model.command_pending = false;
                self.model.error = Some(e);
                return;
            }
        };
        let seconds = self.model.preferences.countdown;
        let services = self.services.clone();
        let interaction_metadata = self.model.preferences.interaction_metadata;
        let performance = self.model.preferences.performance;
        let intent = Arc::new(StartIntent::default());
        if state == RecorderState::Idle {
            self.start_intent = Some(intent.clone());
        }
        let worker_intent = intent.clone();
        let task = cx.background_executor().spawn(async move {
            match state {
                RecorderState::Idle => {
                    let _gate = worker_intent.gate.lock().unwrap_or_else(|p| p.into_inner());
                    if worker_intent.cancelled.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    let request = match configuration {
                        Some(configuration) => configuration.engine_request()?,
                        None => request.expect("validated selection"),
                    };
                    services
                        .control
                        .configure_interaction_metadata(interaction_metadata)?;
                    #[cfg(target_os = "windows")]
                    services.control.configure_performance(performance)?;
                    let result = services.control.start_with_countdown(request, seconds);
                    worker_intent
                        .admitted
                        .store(result.is_ok(), Ordering::Release);
                    result
                }
                RecorderState::Preparing | RecorderState::Recording | RecorderState::Paused => {
                    services.control.stop()
                }
                RecorderState::Failed(_) => services.control.reset(),
                RecorderState::Finalizing => Ok(()),
            }
        });
        cx.spawn_in(window, async move |view, cx| {
            let result = task.await;
            let _ = view.update_in(cx, |this, _, cx| {
                if let Err(e) = result {
                    this.error(e);
                    this.model.command_pending = false;
                    this.start_intent = None;
                }
                if intent.cancelled.load(Ordering::Acquire)
                    && !intent.admitted.load(Ordering::Acquire)
                {
                    this.model.command_pending = false;
                    this.start_intent = None;
                }
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn pause(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.command_pending || !self.model.snapshot.capabilities.can_pause {
            return;
        }
        let control = self.services.control.clone();
        let state = self.model.snapshot.state.clone();
        if !matches!(state, RecorderState::Recording | RecorderState::Paused) {
            return;
        }
        self.model.command_pending = true;
        let task = cx.background_executor().spawn(async move {
            if state == RecorderState::Paused {
                control.resume()
            } else {
                control.pause()
            }
        });
        cx.spawn_in(window, async move |view, cx| {
            let result = task.await;
            let _ = view.update_in(cx, |this, _, cx| {
                if let Err(e) = result {
                    this.error(e);
                    this.model.command_pending = false;
                }
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.close_source_menu(window, cx) {
            return;
        }
        if self.camera_ui.editing {
            if let Some((_, original, _)) = self.camera_ui.drag.take() {
                if let Some(camera) = self.model.request.as_mut().and_then(|r| r.camera.as_mut()) {
                    camera.placement = original;
                }
            }
            self.camera_ui.editing = false;

            cx.notify();
            return;
        }
        if self.close_panel_menu(window, cx) {
            return;
        }
        if self.cancel_workspace_drag(window, cx) {
            return;
        }
        if self.shortcut_edit.is_some() {
            self.cancel_shortcut_capture(cx);
            window.focus(&self.focus, cx);
            cx.notify();
            return;
        }
        if self.number_editor.take().is_some() {
            window.focus(&self.focus, cx);
            cx.notify();
            return;
        }
        if let Some(intent) = self.start_intent.clone()
            && matches!(
                self.model.snapshot.state,
                RecorderState::Idle | RecorderState::Preparing
            )
        {
            intent.cancelled.store(true, Ordering::Release);
            let control = self.services.control.clone();
            let task = cx.background_executor().spawn(async move {
                let _gate = intent.gate.lock().unwrap_or_else(|p| p.into_inner());
                if intent.admitted.load(Ordering::Acquire) {
                    control.stop()
                } else {
                    Ok(())
                }
            });
            cx.spawn_in(window, async move |view, cx| {
                let result = task.await;
                let _ = view.update_in(cx, |this, _, cx| {
                    if let Err(e) = result {
                        this.error(e);
                        this.model.command_pending = false;
                    }
                    cx.notify();
                });
            })
            .detach();
            cx.notify();
            return;
        }
        if self.model.sheet == Some(Sheet::Settings) && self.has_unsaved_settings() {
            self.dispatch(Action::CloseSheet, window, cx);
            return;
        }
        let had_sheet = self.model.sheet.take().is_some();
        if had_sheet {
            self.audio_ui.bitrate_draft = None;
            window.focus(&self.focus, cx);
        }
        if !had_sheet && self.model.snapshot.state == RecorderState::Preparing {
            self.dispatch(Action::Record, window, cx);
        }
        cx.notify();
    }
    pub(super) fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_unsaved_settings()
            && self.save_state.error.is_some()
            && !self.save_state.exit_confirmed
        {
            self.save_state.leave = Some(Action::Exit);
            cx.notify();
            return;
        }

        if self.model.closing {
            return;
        }
        self.model.closing = true;
        cx.notify();
        let control = self.services.control.clone();
        let stop_control = control.clone();
        cx.spawn_in(window, async move |view, cx| {
            let intent = view
                .update_in(cx, |this, window, cx| {
                    this.meter_task = None;
                    #[cfg(target_os = "windows")]
                    if let Some(preview) = &this.preview {
                        preview.update(cx, |preview, cx| preview.close(window, cx));
                    }
                    let intent = this.start_intent.clone();
                    if let Some(intent) = &intent {
                        intent.cancelled.store(true, Ordering::Release);
                    }
                    intent
                })
                .ok()
                .flatten();
            while view
                .update(cx, |this, cx| {
                    #[cfg(target_os = "windows")]
                    {
                        this.preview
                            .as_ref()
                            .is_some_and(|p| !p.read(cx).is_closed())
                    }
                    #[cfg(not(target_os = "windows"))]
                    {
                        let _ = cx;
                        false
                    }
                })
                .unwrap_or(false)
            {
                cx.background_executor()
                    .timer(Duration::from_millis(20))
                    .await;
            }
            let task = cx.background_executor().spawn(async move {
                // Serialize with a countdown/start already admitted by the UI.
                let _gate = intent
                    .as_ref()
                    .map(|i| i.gate.lock().unwrap_or_else(|p| p.into_inner()));
                if matches!(
                    stop_control.published().state,
                    RecorderState::Preparing | RecorderState::Recording | RecorderState::Paused
                ) {
                    stop_control
                        .stop()
                        .map_err(|e| format!("{} {}", e.message, e.action))?;
                }
                // stop() acknowledges admission; publication reaches Idle only after
                // encoder/mux finalization and queued replay saves have drained.
                while matches!(
                    stop_control.published().state,
                    RecorderState::Preparing
                        | RecorderState::Recording
                        | RecorderState::Paused
                        | RecorderState::Finalizing
                ) {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok::<(), String>(())
            });
            if let Err(error) = task.await {
                let _ = view.update(cx, |this, cx| {
                    this.model.closing = false;

                    this.model.error = Some(error);
                    cx.notify();
                });
                return;
            }
            loop {
                let done = view
                    .update_in(cx, |this, window, cx| {
                        this.model.apply_published(this.services.control.published());
                        this.flush_save(cx);
                        if this.save_state.error.is_some()
                            || (this.model.settings_blocked && this.save_latest.is_some())
                        {
                            this.model.closing = false;

                            this.model.sheet = Some(Sheet::Settings);
                            this.save_state.leave = Some(Action::Exit);
                            this.save_state.exit_confirmed = false;
                            if this.model.error.is_none() {
                                this.model.error = Some(
                                    "Settings could not be saved. Retry saving or reload settings before closing."
                                        .into(),
                                );
                            }
                            window.activate_window();
                            window.focus(&this.focus, cx);
                            cx.notify();
                            return 2;
                        }
                        if this.save_running
                            || this.save_latest.is_some()
                            || this.model.shortcuts_pending
                            || this.model.recovery_busy
                            || this.startup.pending
                        {
                            return 0;
                        }
                        1
                    })
                    .unwrap_or(2);
                if done == 2 {
                    return;
                }
                if done == 1 {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(20))
                    .await;
            }
            cx.background_executor()
                .spawn(async move {
                    control.shutdown();
                })
                .await;
            let _ = view.update_in(cx, |this, window, _| {
                this.hotkeys = None;
                window.remove_window();
            });
        })
        .detach();
    }
}
