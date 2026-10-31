//! Application-wide palette selection on GPUI's UI thread.
use serde::{Deserialize, Serialize};
use std::cell::Cell;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeId {
    #[serde(alias = "classic_light")]
    White,
    #[default]
    #[serde(
        alias = "amoled_black",
        alias = "classic_dark",
        alias = "violet_motion",
        alias = "ember_motion",
        alias = "raycast_glass",
        alias = "freym_experimental",
        other
    )]
    Black,
}
thread_local! {
    static ACTIVE: Cell<ThemeId> = const { Cell::new(ThemeId::Black) };
    static ACCENT: Cell<Option<u32>> = const { Cell::new(None) };
}
impl ThemeId {
    pub const ALL: [Self; 2] = [Self::Black, Self::White];
    pub const fn label(self) -> &'static str {
        match self {
            Self::Black => "Black",
            Self::White => "White",
        }
    }
    pub fn current() -> Self {
        ACTIVE.get()
    }
    pub(crate) fn activate(self) {
        ACTIVE.set(self);
    }
}
pub(crate) fn set_accent(color: Option<u32>) {
    ACCENT.set(color.filter(|v| *v <= 0xffffff));
}
pub(super) fn accent() -> Option<u32> {
    ACCENT.get()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn removed_palettes_migrate_without_losing_recording_or_surface_preferences() {
        for name in [
            "amoled_black",
            "classic_dark",
            "violet_motion",
            "ember_motion",
            "raycast_glass",
            "freym_experimental",
            "future_theme",
        ] {
            let mut prefs: crate::state::Preferences = serde_json::from_value(serde_json::json!({
                "theme": name, "surface": "opaque", "countdown": 7, "appearance_revision": 1,
                "artwork": true, "future_option": {"keep": true}
            }))
            .unwrap();
            assert!(prefs.migrate_appearance());
            assert_eq!(prefs.theme, ThemeId::Black);
            assert_eq!(prefs.surface, crate::ui::theme::SurfacePreference::Opaque);
            assert_eq!(prefs.countdown, 7);
            assert!(prefs.artwork);
            assert_eq!(prefs.extra_preferences["future_option"]["keep"], true);
            assert_eq!(serde_json::to_value(prefs).unwrap()["theme"], "black");
        }
        assert_eq!(
            serde_json::from_str::<ThemeId>("\"classic_light\"").unwrap(),
            ThemeId::White
        );
    }
}
