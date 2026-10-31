pub(crate) const DISPLAY_VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"));
mod assets;
mod audio_adapter;
mod home;
mod input;
mod meter;
#[cfg(target_os = "windows")]
mod preview;
mod preview_activity;
mod region_picker;
mod services;
pub mod state;
pub mod ui;
pub(crate) mod video;
pub use assets::{Assets, install_fonts};
pub use home::Home;

pub fn bind_keys(cx: &mut gpui::App) {
    input::bind_keys(cx);
    cx.bind_keys([
        gpui::KeyBinding::new("escape", home::Cancel, Some("FREYM")),
        gpui::KeyBinding::new(
            "escape",
            ui::primitives::DismissOverlay,
            Some("FREYMOverlay"),
        ),
        gpui::KeyBinding::new("tab", ui::primitives::FocusNext, Some("FREYM")),
        gpui::KeyBinding::new("shift-tab", ui::primitives::FocusPrevious, Some("FREYM")),
    ]);
}
