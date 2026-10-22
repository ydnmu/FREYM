use crate::ui::{
    primitives::{ButtonVariant, IconButton, Icons, Text, TextVariant},
    theme::{
        colors::Color,
        spacing::{Layout, Spacing},
    },
};
use gpui::{
    AnyElement, App, IntoElement, RenderOnce, Window, WindowControlArea, div, prelude::*, px,
};
#[derive(IntoElement)]
pub struct Titlebar {
    title: gpui::SharedString,
    close: AnyElement,
    actions: Option<AnyElement>,
    status: Option<AnyElement>,
}
impl Titlebar {
    pub fn new(close: impl IntoElement) -> Self {
        Self {
            title: "FREYM".into(),
            close: close.into_any_element(),
            actions: None,
            status: None,
        }
    }
    pub fn title(mut self, title: impl Into<gpui::SharedString>) -> Self {
        self.title = title.into();
        self
    }
    pub fn actions(mut self, actions: impl IntoElement) -> Self {
        self.actions = Some(actions.into_any_element());
        self
    }
    pub fn status(mut self, status: impl IntoElement) -> Self {
        self.status = Some(status.into_any_element());
        self
    }
}
impl RenderOnce for Titlebar {
    fn render(self, window: &mut Window, _: &mut App) -> impl IntoElement {
        let maximized = window.is_maximized();
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .h(px(Layout::TITLEBAR_HEIGHT))
            .px(Spacing::Lg.px())
            .gap(Spacing::Xs.px())
            .child(
                div()
                    .window_control_area(WindowControlArea::Drag)
                    .h_full()
                    .flex_1()
                    .flex()
                    .items_center()
                    .gap(Spacing::Xs.px())
                    .child(Text::new(self.title).variant(TextVariant::Heading)),
            )
            .when_some(self.status, |d, status| d.child(status))
            .when_some(self.actions, |d, actions| {
                d.child(actions).child(
                    div()
                        .mx(Spacing::Xs.px())
                        .w(gpui::px(1.))
                        .h(Spacing::Lg.px())
                        .bg(Color::DefaultBorder.get()),
                )
            })
            .child(
                IconButton::new("minimize", Icons::Minimize, "Minimize")
                    .window_control(WindowControlArea::Min)
                    .on_click(|_, w, _| w.minimize_window()),
            )
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
            .child(self.close)
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
