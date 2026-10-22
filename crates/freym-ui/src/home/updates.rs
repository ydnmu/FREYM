use super::*;

impl Home {
    pub(super) fn start_updates(&mut self, cx: &mut Context<Self>) {
        if self.update_monitor.is_some() {
            return;
        }
        self.update_monitor = Some(cx.spawn(async move |view, cx| {
            // Recorder creation and local settings/discovery finish independently
            // of the feed. No network or payload IO runs on the GPUI thread.
            cx.background_executor().timer(Duration::from_secs(5)).await;
            let Ok(enabled) = view.update(cx, |this, _| this.model.preferences.automatic_updates)
            else {
                return;
            };
            let worker = cx
                .background_executor()
                .spawn(async move { freym_update::Updater::new(enabled).map(Arc::new) });
            let result = worker.await;
            if view
                .update(cx, |this, cx| {
                    match result {
                        Ok(updater) => {
                            if enabled != this.model.preferences.automatic_updates {
                                updater.set_enabled(this.model.preferences.automatic_updates);
                            }
                            this.update_snapshot = updater.snapshot();
                            this.updates = Some(updater);
                        }
                        Err(error) => {
                            this.update_snapshot.state =
                                freym_update::State::Failed(error.to_string())
                        }
                    }
                    cx.notify();
                })
                .is_err()
            {
                return;
            }
            loop {
                if view
                    .update(cx, |this, cx| {
                        if let Some(updater) = &this.updates {
                            let snapshot = updater.snapshot();
                            if this.update_snapshot != snapshot {
                                this.update_snapshot = snapshot;
                                if this.model.sheet == Some(Sheet::Settings) {
                                    cx.notify();
                                }
                            }
                            if !this.model.closing && !this.update_running && updater.due() {
                                this.check_updates(false, cx);
                            }
                        }
                    })
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(1)).await;
            }
        }));
    }
    pub(super) fn check_updates(&mut self, manual: bool, cx: &mut Context<Self>) {
        if self.update_running || self.model.closing {
            return;
        }
        let Some(updater) = self.updates.clone() else {
            return;
        };
        self.update_running = true;
        let worker = cx.background_executor().spawn(async move {
            if manual {
                updater.check_now();
            } else {
                updater.check();
            }
        });
        cx.spawn(async move |view, cx| {
            worker.await;
            let _ = view.update(cx, |this, cx| {
                this.update_running = false;
                if let Some(updater) = &this.updates {
                    this.update_snapshot = updater.snapshot();
                }
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn toggle_updates(&mut self, cx: &mut Context<Self>) {
        if !self.preferences_loaded || self.model.closing {
            return;
        }
        self.model.preferences.automatic_updates = !self.model.preferences.automatic_updates;
        if let Some(updater) = &self.updates {
            updater.set_enabled(self.model.preferences.automatic_updates);
            self.update_snapshot = updater.snapshot();
        }
        self.persist(cx);
    }
}
