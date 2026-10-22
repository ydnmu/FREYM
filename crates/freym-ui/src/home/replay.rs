use super::*;

impl Home {
    pub(super) fn toggle_replay(&mut self, cx: &mut Context<Self>) {
        if self.model.command_pending || self.model.recovery_busy || self.model.closing {
            return;
        }
        let stop = self.model.snapshot.replay_only;
        if !stop && (self.save_running || self.save_latest.is_some()) {
            return;
        }
        if !stop && (!self.model.can_start()) {
            return;
        }
        if !stop && let Err(error) = self.validate_audio_recording() {
            self.error(error);
            cx.notify();
            return;
        }
        self.model.sync_routes();
        let Some(mut request) = self.model.request.clone() else {
            return;
        };
        if !stop && let Some(document) = &self.settings_document {
            let result = document.configuration().and_then(|mut c| {
                c.request = request.clone();
                c.engine_request().map_err(|e| e.message)
            });
            match result {
                Ok(r) => request = r,
                Err(e) => {
                    self.model.error = Some(e);
                    cx.notify();
                    return;
                }
            }
        }
        request.output.file_stem = frame_runtime::recording_control::AUTO_FILE_STEM.into();
        if !stop {
            self.model.notice =
                Some("Replay will be ready after the selected interval has buffered.".into());
        }
        let services = self.services.clone();
        self.model.command_pending = true;
        self.model.error = None;
        let task = cx.background_executor().spawn(async move {
            if stop {
                services.control.stop_replay()
            } else {
                services.control.start_replay(request)
            }
        });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.error(error);
                    this.model.command_pending = false;
                }
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn configure_replay(&mut self, cx: &mut Context<Self>) {
        let mut settings = self.model.snapshot.replay_settings;
        settings.duration_seconds = match settings.duration_seconds {
            15 => 30,
            30 => 60,
            60 => 120,
            120 => 300,
            300 => 600,
            _ => 15,
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
    pub(super) fn save_replay(&mut self, cx: &mut Context<Self>) {
        if !self.model.can_save_replay() {
            if self.model.snapshot.replay_active && self.model.replay_wait_seconds() > 0 {
                self.model.notice = Some(format!(
                    "Keep recording for {}s to save this replay interval.",
                    self.model.replay_wait_seconds()
                ));
                cx.notify();
            }
            return;
        }
        let Some(r) = &self.model.request else {
            return;
        };
        let path = std::path::Path::new(&r.output.directory).join(format!(
            "replay-{}.mkv",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        ));
        let services = self.services.clone();
        self.model.error = None;
        self.model.notice = Some("Saving replay…".into());
        self.model.replay_save_pending = true;
        let seconds = self.model.snapshot.replay_settings.duration_seconds;
        let task = cx
            .background_executor()
            .spawn(async move { services.control.save_replay(path, seconds) });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                if let Err(e) = result {
                    this.model.replay_save_pending = false;
                    this.model.notice = None;
                    this.error(e);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}
