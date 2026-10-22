mod assets;
mod audio_adapter;
mod home;
mod input;
mod meter;
#[cfg(target_os = "windows")]
mod preview;
mod region_picker;
mod services;
pub mod state;
mod studio;
pub mod ui;
pub(crate) mod video;
pub use assets::{Assets, install_fonts};
pub use home::Home;
#[cfg(feature = "ui-preview")]
pub use home::ui_preview::PreviewWorkbench;
pub fn bind_keys(cx: &mut gpui::App) {
    input::bind_keys(cx);
    cx.bind_keys([
        gpui::KeyBinding::new("ctrl-shift-r", home::Record, Some("FREYM")),
        gpui::KeyBinding::new("ctrl-shift-p", home::PauseResume, Some("FREYM")),
        gpui::KeyBinding::new("escape", home::Cancel, Some("FREYM")),
        gpui::KeyBinding::new(
            "escape",
            ui::primitives::DismissOverlay,
            Some("FREYMOverlay"),
        ),
        gpui::KeyBinding::new("ctrl-,", home::OpenSettings, Some("FREYM")),
        gpui::KeyBinding::new("f11", home::Fullscreen, Some("FREYM")),
        gpui::KeyBinding::new("tab", ui::primitives::FocusNext, Some("FREYM")),
        gpui::KeyBinding::new("shift-tab", ui::primitives::FocusPrevious, Some("FREYM")),
    ]);
}
