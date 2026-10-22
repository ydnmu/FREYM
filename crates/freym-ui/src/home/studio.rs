use super::*;
impl Home {
    pub(super) fn open_studio_path(&mut self, source: std::path::PathBuf, cx: &mut Context<Self>) {
        if self.model.closing || self.studio_prompt_pending {
            return;
        }
        let source = std::fs::canonicalize(&source).unwrap_or(source);
        self.studios
            .retain(|_, handle| handle.update(cx, |_, _, _| ()).is_ok());
        if let Some(handle) = self.studios.get(&source) {
            let _ = handle.update(cx, |studio, window, cx| studio.activate(window, cx));
            return;
        }
        if let Some(handle) = self.studios.values().next().copied() {
            self.studio_prompt_pending = true;
            let (send, receive) = async_channel::bounded(1);
            let _ = handle.update(cx, |studio, window, cx| {
                studio.request_close(send, window, cx)
            });
            cx.spawn(async move |view, cx| {
                let closed = receive.recv().await == Ok(true);
                let _ = view.update(cx, |this, cx| {
                    this.studio_prompt_pending = false;
                    if closed {
                        this.open_studio_path(source, cx);
                    }
                });
            })
            .detach();
            return;
        }
        self.create_studio(source, cx);
    }
    fn create_studio(&mut self, source: std::path::PathBuf, cx: &mut Context<Self>) {
        let key = source.clone();
        let editor_source = source.clone();
        let owner = cx.weak_entity();
        let owner_window = self.owner_window;
        let bounds = gpui::Bounds::centered(None, gpui::size(px(1000.), px(820.)), cx);
        let result = cx.open_window(
            gpui::WindowOptions {
                window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("FREYM Studio".into()),
                    appears_transparent: true,
                    ..Default::default()
                }),
                window_min_size: Some(gpui::size(px(760.), px(620.))),
                ..Default::default()
            },
            |window, cx| {
                cx.new(|cx| {
                    crate::studio::Studio::new(
                        editor_source,
                        move |cx| {
                            // A clean Studio can close synchronously while Home is
                            // opening another source. Defer to avoid reborrowing Home.
                            cx.defer(move |cx| {
                                let focus = owner.update(cx, |this, cx| {
                                    this.studios.remove(&key);
                                    cx.notify();
                                    (!this.model.closing).then(|| this.focus.clone())
                                });
                                if let Ok(Some(focus)) = focus {
                                    let _ = owner_window.update(cx, |_, window, cx| {
                                        window.activate_window();
                                        window.focus(&focus, cx);
                                    });
                                }
                            });
                        },
                        window,
                        cx,
                    )
                })
            },
        );
        match result {
            Ok(handle) => {
                self.studios.insert(source, handle);
            }
            Err(error) => {
                self.model.error = Some(error.to_string());
                cx.notify();
            }
        }
    }
    pub(super) fn open_studio(&mut self, cx: &mut Context<Self>) {
        if !self.model.editable() || self.studio_prompt_pending {
            return;
        }
        self.studio_prompt_pending = true;
        let answer = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open recording in Freym Studio".into()),
        });
        cx.spawn(async move |view, cx| {
            let source = match answer.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                _ => None,
            };
            let _ = view.update(cx, |this, cx| {
                this.studio_prompt_pending = false;
                if let Some(source) = source {
                    this.open_studio_path(source, cx);
                }
            });
        })
        .detach();
    }
}
