use gpui::{Pixels, px};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spacing {
    None,
    Xxs,
    Xs,
    Sm,
    Md,
    Lg,
    Xl,
    Xxl,
}
impl Spacing {
    pub const fn value(self) -> f32 {
        match self {
            Self::None => 0.,
            Self::Xxs => 4.,
            Self::Xs => 8.,
            Self::Sm => 12.,
            Self::Md => 16.,
            Self::Lg => 24.,
            Self::Xl => 32.,
            Self::Xxl => 48.,
        }
    }
    pub fn px(self) -> Pixels {
        px(self.value())
    }
}
pub struct Layout;
impl Layout {
    pub const CONTROL_HEIGHT: f32 = 36.;
    pub const COMPACT_CONTROL: f32 = 36.;
    pub const ICON_CONTROL: f32 = 40.;
    pub const ICON: f32 = 18.;
    pub const ICON_SMALL: f32 = 14.;
    pub const ICON_LARGE: f32 = 24.;
    pub const TITLEBAR_HEIGHT: f32 = 48.;
    pub const RECORD_WIDTH: f32 = 254.;
    pub const RECORD_HEIGHT: f32 = 54.;
    pub const SHEET_WIDTH: f32 = 510.;
    pub const SHEET_MAX_HEIGHT: f32 = 680.;
    pub const EDITOR_WIDTH: f32 = 304.;
    pub const MIN_WINDOW_WIDTH: f32 = 800.;
    pub const MIN_WINDOW_HEIGHT: f32 = 640.;
    pub const WINDOW_WIDTH: f32 = 1280.;
    pub const WINDOW_HEIGHT: f32 = 850.;
    pub const HOME_MAX_WIDTH: f32 = 1180.;
    pub const MEDIA_MIN_HEIGHT: f32 = 200.;
    pub const MEDIA_MAX_HEIGHT: f32 = 480.;
    pub const AUDIO_PANEL_WIDTH: f32 = 380.;
    pub const AUDIO_PANEL_COMPACT_WIDTH: f32 = 320.;
    pub const CARD_HEIGHT: f32 = 76.;
    pub const CARD_COMPACT_HEIGHT: f32 = 62.;
    pub const COMPACT_BREAKPOINT: f32 = 960.;
    pub const AUDIO_MIN_WIDTH: f32 = 148.;
    pub const TOGGLE_WIDTH: f32 = 28.;
    pub const TOGGLE_HEIGHT: f32 = 16.;
    pub const TOGGLE_THUMB: f32 = 12.;
    pub const TOGGLE_INSET: f32 = 2.;
    pub const INPUT_LINE: f32 = 30.;
    pub const METER_WIDTH: f32 = 104.;
    pub const METER_HEIGHT: f32 = 22.;
    pub const PREVIEW_INSET: f32 = 18.;
}
