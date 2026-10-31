use super::*;

#[derive(Default)]
pub(super) struct StartupState {
    pub enabled: Option<bool>,
    pub pending: bool,
    pub failed: bool,
}
impl StartupState {
    #[cfg(test)]
    pub fn fixture() -> Self {
        Self {
            enabled: Some(false),
            ..Default::default()
        }
    }
}
impl Home {
    /// Ordinary startup uses the desktop; named profiles remain explicit choices.
    pub(super) fn apply_startup_source_defaults(&mut self, explicit_profile: bool) {
        if explicit_profile {
            return;
        }
        if let Some(display) = self
            .model
            .snapshot
            .displays
            .iter()
            .find(|display| display.is_primary)
            .or_else(|| self.model.snapshot.displays.first())
            && let Some(request) = &mut self.model.request
        {
            request.target = CaptureTarget::Display(display.id.clone());
            request.resolution = Resolution::Native;
            if let Some(quality) = display.required_recording_quality {
                request.quality = quality;
            }

            self.model.mode = SourceMode::Screen;
        }
        self.suspend_startup_camera();
    }

    pub(super) fn load_startup(&mut self, cx: &mut Context<Self>) {
        self.change_startup(None, cx);
    }
    pub(super) fn toggle_startup(&mut self, cx: &mut Context<Self>) {
        if !self.preferences_loaded || self.model.closing || self.startup.pending {
            return;
        }
        if let Some(enabled) = self.startup.enabled {
            self.startup.enabled = Some(!enabled);
            self.save_state.startup_dirty = true;
            self.persist_mode(cx);
            cx.notify();
        }
    }
    fn change_startup(&mut self, enabled: Option<bool>, cx: &mut Context<Self>) {
        #[cfg(test)]
        {
            #[cfg(test)]
            let fixture = true;
            if fixture {
                self.startup.enabled = Some(enabled.unwrap_or(false));
                cx.notify();
                return;
            }
        }
        self.startup.pending = true;
        let task = cx.background_executor().spawn(async move {
            match enabled {
                Some(value) => freym_platform::startup::set_enabled(value),
                None => freym_platform::startup::enabled(),
            }
        });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |home, cx| {
                home.startup.pending = false;
                home.startup.failed = result.is_err();
                match result {
                    Ok(value) => home.startup.enabled = Some(value),
                    Err(error) => home.model.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}
