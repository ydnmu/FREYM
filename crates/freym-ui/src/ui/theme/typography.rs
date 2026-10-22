use gpui::{FontWeight, Pixels, px};
pub const FONT_FAMILY: &str = "Freym Lab Sans";
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Typography {
    Title,
    Heading,
    Body,
    Label,
    Caption,
}
impl Typography {
    pub fn size(self) -> Pixels {
        px(match self {
            Self::Title => 26.,
            Self::Heading => 20.,
            Self::Body => 14.,
            Self::Label => 13.,
            Self::Caption => 12.,
        })
    }
    pub fn weight(self) -> FontWeight {
        if matches!(self, Self::Title | Self::Heading) {
            return FontWeight::SEMIBOLD;
        }
        match self {
            Self::Label => FontWeight::MEDIUM,
            _ => FontWeight::NORMAL,
        }
    }
}
