//! Delayed native GPUI tooltip; a single small still, no live UI animation loop.
use crate::ui::theme::colors::Color;
use gpui::{Context, Window, div, prelude::*, px};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(crate) struct SourceThumbnail {
    image: Option<Arc<gpui::RenderImage>>,
    message: String,
    cancelled: Arc<AtomicBool>,
    _task: Option<gpui::Task<()>>,
}
pub(crate) type ThumbnailRequest = Box<dyn FnOnce(&AtomicBool) -> Result<Vec<u8>, String> + Send>;
impl SourceThumbnail {
    pub(crate) fn new(fetch: ThumbnailRequest, cx: &mut Context<Self>) -> Self {
        let cancelled = Arc::new(AtomicBool::new(false));
        #[cfg(all(target_os = "windows", not(test)))]
        let task = {
            let cancel = cancelled.clone();
            let renderer = cx.svg_renderer().clone();
            let request = cx.background_executor().spawn(async move {
                let bytes = fetch(&cancel)?;
                if cancel.load(Ordering::Relaxed) {
                    return Err("Preview cancelled".into());
                }
                gpui::Image::from_bytes(gpui::ImageFormat::Png, bytes)
                    .to_image_data(renderer)
                    .map_err(|e| e.to_string())
            });
            Some(cx.spawn(async move |view, cx| {
                let result = request.await;
                let _ = view.update(cx, |this, cx| {
                    match result {
                        Ok(image) => this.image = Some(image),
                        Err(error) => this.message = error,
                    }
                    cx.notify();
                });
            }))
        };
        #[cfg(any(not(target_os = "windows"), test))]
        let task = {
            let _ = (fetch, cx);
            None
        };
        Self {
            image: None,
            message: "Loading preview…".into(),
            cancelled,
            _task: task,
        }
    }
}
impl Drop for SourceThumbnail {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}
impl gpui::Render for SourceThumbnail {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("source-hover-preview")
            .debug_selector(|| "source-hover-preview".into())
            .w(px(296.))
            .p(px(8.))
            .rounded(px(6.))
            .bg(Color::DockSurface.get())
            .border_1()
            .border_color(Color::SubtleBorder.get())
            .child(
                div()
                    .w_full()
                    .h(px(158.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(11.))
                    .text_color(Color::SecondaryText.get())
                    .map(|d| {
                        if let Some(image) = &self.image {
                            d.child(
                                gpui::img(image.clone())
                                    .size_full()
                                    .object_fit(gpui::ObjectFit::Contain),
                            )
                        } else {
                            d.child(self.message.clone())
                        }
                    }),
            )
            .child(
                div()
                    .mt(px(6.))
                    .text_size(px(10.))
                    .text_color(Color::SecondaryText.get())
                    .child("Drag to Preview to select this source"),
            )
    }
}
