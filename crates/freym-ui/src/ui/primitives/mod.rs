pub mod button;
mod card_light;
pub(crate) mod control_scope;
pub mod icon;
mod icon_button;
pub(crate) mod motion_press;
mod separator;
mod stack;
mod surface;
mod text;
pub use crate::input::TextInput;
pub use button::{Button, ButtonSize, ButtonVariant};
pub use icon::{Icon, IconSize, Icons};
pub use icon_button::IconButton;
pub use separator::Separator;
pub use stack::{Alignment, Stack};
pub use surface::{Surface, SurfaceVariant};
pub use text::{Text, TextVariant};
gpui::actions!(freym_ui, [FocusNext, FocusPrevious, DismissOverlay]);

// Product controls delegate all interaction to the same primitive.
macro_rules! clickable {
    ($component:ty) => {
        impl $component {
            pub fn disabled(mut self, disabled: bool) -> Self {
                self.button = self.button.disabled(disabled);
                self
            }
            pub fn selected(mut self, selected: bool) -> Self {
                self.button = self.button.selected(selected);
                self
            }
            pub fn on_click(
                mut self,
                handler: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
            ) -> Self {
                self.button = self.button.on_click(handler);
                self
            }
        }
    };
}
pub(crate) use clickable;
