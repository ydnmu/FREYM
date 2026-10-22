use super::*;

impl Home {
    pub(super) fn load_cameras(&mut self, cx: &mut Context<Self>) {
        #[cfg(target_os = "windows")]
        {
            let task = cx
                .background_executor()
                .spawn(async { frame_windows::camera::enumerate_cameras() });
            cx.spawn(async move |view, cx| {
                let r = task.await;
                let _ = view.update(cx, |this, cx| {
                    match r {
                        Ok(c) => this.model.cameras = c,
                        Err(e) => this.error(e),
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = cx;
            self.model.notice = Some("Camera overlay is unavailable on this platform.".into());
        }
    }
    pub(super) fn select_camera(&mut self, id: String, cx: &mut Context<Self>) {
        if !self.model.editable() {
            return;
        }
        #[cfg(target_os = "windows")]
        {
            self.model.command_pending = true;
            let task = cx.background_executor().spawn(async move {
                frame_windows::camera::enumerate_camera_formats(&id).map(|f| (id, f))
            });
            cx.spawn(async move |view, cx| {
                let result = task.await;
                let _ = view.update(cx, |this, cx| {
                    this.model.command_pending = false;
                    match result {
                        Ok((id, formats)) => {
                            if this.model.closing
                                || this.model.snapshot.state != RecorderState::Idle
                            {
                                return;
                            }
                            this.model.formats = formats;
                            if let Some(f) = this.model.formats.iter().find(|f| f.supported)
                                && let Some(r) = &mut this.model.request
                            {
                                r.camera = Some(CameraSelection {
                                    device_id: id,
                                    format_index: f.index,
                                    placement: CameraPlacement {
                                        x: 16,
                                        y: 16,
                                        width: 320,
                                        height: 180,
                                    },
                                });
                                this.persist(cx);
                            }
                        }
                        Err(e) => this.error(e),
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (id, cx);
        }
    }
}
