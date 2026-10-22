//! Application-wide palette selection on GPUI's UI thread. No renderer resources.
use serde::{Deserialize, Serialize};
use std::cell::Cell;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeId {
    #[default]
    AmoledBlack,
    FreymExperimental,
    RaycastGlass,
    ClassicDark,
    VioletMotion,
    EmberMotion,
    #[serde(other)]
    ClassicLight,
}
thread_local! {
    static ACTIVE: Cell<ThemeId> = const { Cell::new(ThemeId::AmoledBlack) };
}
impl ThemeId {
    pub const ALL: [Self; 7] = [
        Self::AmoledBlack,
        Self::ClassicDark,
        Self::ClassicLight,
        Self::VioletMotion,
        Self::EmberMotion,
        Self::FreymExperimental,
        Self::RaycastGlass,
    ];
    pub const fn label(self) -> &'static str {
        match self {
            Self::AmoledBlack => "AMOLED Black",
            Self::FreymExperimental => "FREYM Experimental",
            Self::RaycastGlass => "Raycast Glass",
            Self::ClassicDark => "Charcoal",
            Self::ClassicLight => "Sky",
            Self::VioletMotion => "Violet Motion",
            Self::EmberMotion => "Ember Motion",
        }
    }
    pub const fn is_motion(self) -> bool {
        matches!(self, Self::VioletMotion | Self::EmberMotion)
    }
    pub fn current() -> Self {
        ACTIVE.get()
    }
    /// Called only when preferences load or the owner changes theme; repaint all windows.
    pub(crate) fn activate(self) {
        ACTIVE.set(self);
    }
}
