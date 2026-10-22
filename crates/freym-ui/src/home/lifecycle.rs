use super::*;

impl Home {
    pub(super) fn error(&mut self, e: RecorderError) {
        self.model.error = Some(format!("{} {}", e.message, e.action));
    }
    pub(super) fn record(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.command_pending || self.model.closing || self.model.recovery_busy {
            return;
        }
        let state = self.model.snapshot.state.clone();
        if state == RecorderState::Idle && (self.save_running || self.save_latest.is_some()) {
            self.model.notice = Some("Wait for settings to finish saving before recording.".into());
            cx.notify();
            return;
        }
        let request = if state == RecorderState::Idle {
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
        if self.shortcut_edit.take().is_some() {
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
        let had_sheet = self.model.sheet.take().is_some();
        if had_sheet {
            window.focus(&self.focus, cx);
        }
        if !had_sheet && self.model.snapshot.state == RecorderState::Preparing {
            self.dispatch(Action::Record, window, cx);
        }
        cx.notify();
    }
    pub(super) fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.restore_compact(window, cx);
        #[cfg(feature = "ui-preview")]
        if self.ui_preview {
            window.remove_window();
            cx.quit();
            return;
        }
        if self.model.closing {
            return;
        }
        self.model.closing = true;
        cx.notify();
        let control = self.services.control.clone();
        let studios: Vec<_> = self.studios.values().copied().collect();
        cx.spawn_in(window, async move |view, cx| {
            // Never destroy the recorder HWND while an editor still owns work.
            // Each Studio drains its tasks and resolves its own save/discard prompt.
            for studio in studios {
                let (send, receive) = async_channel::bounded(1);
                if studio
                    .update(cx, |studio, window, cx| {
                        studio.request_close(send, window, cx)
                    })
                    .is_ok()
                    && receive.recv().await != Ok(true)
                {
                    let _ = view.update_in(cx, |this, window, cx| {
                        this.model.closing = false;
                        window.activate_window();
                        window.focus(&this.focus, cx);
                        cx.notify();
                    });
                    return;
                }
            }
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
                    control.published().state,
                    RecorderState::Preparing | RecorderState::Recording | RecorderState::Paused
                ) {
                    control
                        .stop()
                        .map_err(|e| format!("{} {}", e.message, e.action))?;
                }
                // stop() acknowledges admission; publication reaches Idle only after
                // encoder/mux finalization and queued replay saves have drained.
                while matches!(
                    control.published().state,
                    RecorderState::Preparing
                        | RecorderState::Recording
                        | RecorderState::Paused
                        | RecorderState::Finalizing
                ) {
                    std::thread::sleep(Duration::from_millis(20));
                }
                control.shutdown();
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
                    .update_in(cx, |this, window, _cx| {
                        if this.save_running
                            || this.save_latest.is_some()
                            || this.model.shortcuts_pending
                            || this.model.recovery_busy
                        {
                            return false;
                        }
                        this.hotkeys = None;
                        window.remove_window();
                        true
                    })
                    .unwrap_or(true);
                if done {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(20))
                    .await;
            }
        })
        .detach();
    }
}
