//! Freym's public presentation API. Components never call media services.

pub mod components;

/// Isolated specimens derived from the browser reference, before product adoption.
pub mod overlays;
pub mod primitives;
#[cfg(test)]
mod tests;
pub mod theme;

/// Presentation hooks are inert in the recording application.
pub(crate) fn designable(
    element: gpui::Stateful<gpui::Div>,
    key: &str,
    label: &str,
    cx: &gpui::App,
) -> gpui::Stateful<gpui::Div> {
    {
        let _ = (key, label, cx);
        element
    }
}
pub(crate) fn finish_design(
    element: gpui::Stateful<gpui::Div>,
    key: &str,
    cx: &gpui::App,
) -> gpui::AnyElement {
    {
        use gpui::IntoElement;
        let _ = (key, cx);
        element.into_any_element()
    }
}
pub(crate) fn viewport(window: &gpui::Window, cx: &gpui::App) -> gpui::Size<gpui::Pixels> {
    {
        let _ = cx;
        window.viewport_size()
    }
}
