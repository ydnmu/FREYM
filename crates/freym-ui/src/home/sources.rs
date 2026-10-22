use super::*;

impl Home {
    pub(super) fn pick_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.model.editable() {
            return;
        }
        #[cfg(target_os = "windows")]
        {
            let owner = Self::raw_owner(window);
            freym_platform::set_window_visible(owner, false);
            self.model.command_pending = true;
            let task = cx
                .background_executor()
                .spawn(async move { frame_windows::pick_window_on_screen(owner) });
            cx.spawn_in(window, async move |view, cx| {
                let result = task.await;
                let _ = view.update_in(cx, |this, window, cx| {
                    freym_platform::set_window_visible(owner, true);
                    window.activate_window();
                    this.model.command_pending = false;
                    match result {
                        Ok(Some(source)) => this.choose_window(source.id, cx),
                        Ok(None) => {}
                        Err(e) => this.error(e),
                    }
                    cx.notify();
                });
            })
            .detach();
        }
    }
    pub(super) fn pick_area(
        &mut self,
        display: DisplayInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.model.editable() {
            return;
        }
        let monitor =
            freym_platform::monitor_handle(display.x, display.y, display.width, display.height)
                .and_then(|id| cx.find_display(gpui::DisplayId::new(id)));
        let monitor = monitor.or_else(|| {
            cx.displays().into_iter().find(|d| {
                let b = d.bounds();
                (f32::from(b.size.width) * display.scale_factor as f32 - display.width as f32).abs()
                    < 2.
                    && (f32::from(b.origin.x) * display.scale_factor as f32 - display.x as f32)
                        .abs()
                        < 2.
            })
        });
        let Some(monitor) = monitor else {
            self.model.error = Some(
                "The selected display changed. Refresh sources before selecting an area.".into(),
            );
            return;
        };
        let bounds = monitor.bounds();
        let display_id = monitor.id();
        let owner = Self::raw_owner(window);
        let owner_window = window.window_handle();
        let view = cx.weak_entity();
        freym_platform::set_window_visible(owner, false);
        self.model.command_pending = true;
        let result = cx.open_window(
            gpui::WindowOptions {
                window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
                display_id: Some(display_id),
                // An ordinary native window remains discoverable to assistive
                // tools and Alt-Tab; Windows tool popups are excluded from both.
                kind: gpui::WindowKind::Normal,
                titlebar: None,
                is_resizable: false,
                is_minimizable: false,
                window_background: gpui::WindowBackgroundAppearance::Transparent,
                ..Default::default()
            },
            move |window, cx| {
                cx.new(|cx| {
                    crate::region_picker::RegionPicker::new(
                        display,
                        move |region, cx| {
                            freym_platform::set_window_visible(owner, true);
                            let _ =
                                owner_window.update(cx, |_, window, _| window.activate_window());
                            let _ = view.update(cx, |this, cx| {
                                this.model.command_pending = false;
                                if let Some(region) = region {
                                    if let Some(r) = &mut this.model.request {
                                        r.target = CaptureTarget::Region(region);
                                    }
                                    this.model.mode = SourceMode::Area;
                                    this.model.sheet = None;
                                    this.persist(cx);
                                }
                                cx.notify();
                            });
                        },
                        window,
                        cx,
                    )
                })
            },
        );
        if let Err(e) = result {
            freym_platform::set_window_visible(owner, true);
            self.model.command_pending = false;
            self.model.error = Some(e.to_string());
        }
    }
    pub(super) fn choose_window(&mut self, id: WindowId, cx: &mut Context<Self>) {
        if !self.model.editable() {
            return;
        }
        #[cfg(target_os = "windows")]
        {
            self.model.command_pending = true;
            let task = cx
                .background_executor()
                .spawn(async move { frame_windows::validate_window_selection(&id) });
            cx.spawn(async move |view, cx| {
                let result = task.await;
                let _ = view.update(cx, |this, cx| {
                    this.model.command_pending = false;
                    match result {
                        Ok(w) => {
                            if this.model.snapshot.state == RecorderState::Idle {
                                if let Some(r) = &mut this.model.request {
                                    r.target = CaptureTarget::Window(w.id.clone());
                                    r.camera = None;
                                }
                                if !this.model.snapshot.windows.iter().any(|v| v.id == w.id) {
                                    this.model.snapshot.windows.push(w);
                                }
                                this.model.mode = SourceMode::Window;
                                this.model.sheet = None;
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
            self.model.error =
                Some("Native window selection is unavailable on this platform.".into());
        }
    }
}
