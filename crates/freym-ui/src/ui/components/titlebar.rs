use crate::ui::{
    primitives::{ButtonVariant, IconButton, Icons, Text, TextVariant},
    theme::{
        colors::Color,
        spacing::{Layout, Spacing},
    },
};
use gpui::{
    AnyElement, App, Bounds, IntoElement, Pixels, RenderOnce, SharedString, TitlebarOptions,
    Window, WindowBounds, WindowControlArea, WindowOptions, div, prelude::*, px, size,
};
#[derive(IntoElement)]
pub struct Titlebar {
    title: gpui::SharedString,
    close: AnyElement,
    minimize: Option<AnyElement>,
    actions: Option<AnyElement>,
    status: Option<AnyElement>,
    leading: Option<AnyElement>,
    transparent_background: bool,
}
impl Titlebar {
    /// Ordinary top-level windows reserve their client titlebar for this component.
    /// Popups/selection overlays keep their existing independent window options.
    pub fn window_options(
        title: impl Into<SharedString>,
        bounds: Bounds<Pixels>,
        app_id: impl Into<String>,
    ) -> WindowOptions {
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(title.into()),
                appears_transparent: true,
                ..Default::default()
            }),
            window_min_size: Some(size(
                px(Layout::MIN_WINDOW_WIDTH),
                px(Layout::MIN_WINDOW_HEIGHT),
            )),
            is_movable: true,
            is_resizable: true,
            is_minimizable: true,
            app_id: Some(app_id.into()),
            ..Default::default()
        }
    }
    pub fn new(close: impl IntoElement) -> Self {
        Self {
            title: "FREYM".into(),
            close: close.into_any_element(),
            minimize: None,
            actions: None,
            status: None,
            leading: None,
            transparent_background: false,
        }
    }
    pub fn title(mut self, title: impl Into<gpui::SharedString>) -> Self {
        self.title = title.into();
        self
    }
    /// Recorder windows retain a transparent viewport for their child HWND.
    pub fn transparent_background(mut self) -> Self {
        self.transparent_background = true;
        self
    }
    pub fn actions(mut self, actions: impl IntoElement) -> Self {
        self.actions = Some(actions.into_any_element());
        self
    }
    #[allow(dead_code)]
    pub(crate) fn minimize_button(mut self, button: impl IntoElement) -> Self {
        self.minimize = Some(button.into_any_element());
        self
    }
    pub fn status(mut self, status: impl IntoElement) -> Self {
        self.status = Some(status.into_any_element());
        self
    }
    pub(crate) fn leading(mut self, content: impl IntoElement) -> Self {
        self.leading = Some(content.into_any_element());
        self
    }
}
impl RenderOnce for Titlebar {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        crate::ui::theme::surface_treatment::apply_window(window, cx, self.transparent_background);
        let maximized = window.is_maximized();
        let branded = self.title == "Freym" || self.title.starts_with("FREYM");
        let element = div()
            .id("home-titlebar")
            .debug_selector(|| "home-titlebar".into())
            .w_full()
            .min_w_0()
            .flex()
            .flex_shrink_0()
            .items_center()
            .h(px(Layout::TITLEBAR_HEIGHT))
            .px(Spacing::Lg.px())
            .gap(Spacing::Xs.px())
            .bg(crate::ui::theme::surface_treatment::titlebar_background())
            .when_some(self.leading, |d, leading| d.child(leading))
            .child(
                div()
                    .id("window-drag-region")
                    .debug_selector(|| "window-drag-region".into())
                    .window_control_area(WindowControlArea::Drag)
                    .h_full()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .overflow_hidden()
                    .when(branded, |d| {
                        d.child(
                            div()
                                .id("titlebar-brand-mark")
                                .debug_selector(|| "titlebar-brand-mark".into())
                                .size(px(24.))
                                .flex_shrink_0()
                                .child(crate::assets::brand_mark(24.)),
                        )
                    })
                    .child(
                        div()
                            .id("titlebar-label")
                            .debug_selector(|| "titlebar-label".into())
                            .min_w_0()
                            .text_ellipsis()
                            .when(branded, |d| {
                                d.text_size(px(15.))
                                    .line_height(px(20.))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(Color::PrimaryText.get())
                                    .child(self.title.clone())
                            })
                            .when(!branded && self.title == "Settings", |d| {
                                d.text_size(px(18.))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(Color::PrimaryText.get())
                                    .child(self.title.clone())
                            })
                            .when(!branded && self.title != "Settings", |d| {
                                d.child(Text::new(self.title).variant(TextVariant::Heading))
                            }),
                    ),
            )
            .when_some(self.status, |d, status| d.child(status))
            .when_some(self.actions, |d, actions| d.child(actions))
            .child(self.minimize.unwrap_or_else(|| {
                IconButton::new("minimize", Icons::Minimize, "Minimize")
                    .window_control(WindowControlArea::Min)
                    .on_click(|_, w, _| w.minimize_window())
                    .into_any_element()
            }))
            .child(
                IconButton::new(
                    "maximize",
                    if maximized {
                        Icons::Restore
                    } else {
                        Icons::Maximize
                    },
                    if maximized {
                        "Restore window"
                    } else {
                        "Maximize window"
                    },
                )
                .window_control(WindowControlArea::Max)
                .on_click(|_, w, _| toggle_maximize(w)),
            )
            .child(self.close);
        crate::ui::finish_design(
            crate::ui::designable(element, "home-titlebar", "Title bar", cx),
            "home-titlebar",
            cx,
        )
    }
}
fn toggle_maximize(window: &Window) {
    #[cfg(target_os = "windows")]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if let Ok(handle) = HasWindowHandle::window_handle(window)
            && let RawWindowHandle::Win32(handle) = handle.as_raw()
        {
            freym_platform::toggle_window_maximized(handle.hwnd.get());
        }
    }
    #[cfg(not(target_os = "windows"))]
    window.zoom_window();
}
pub fn close_button(
    handler: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> IconButton {
    IconButton::new("close-window", Icons::Close, "Close FREYM")
        .variant(ButtonVariant::Danger)
        .window_control(WindowControlArea::Close)
        .on_click(handler)
}
