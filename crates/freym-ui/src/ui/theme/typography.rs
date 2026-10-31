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
            Self::Title => 28.,
            Self::Heading => 22.,
            Self::Body | Self::Label => 15.,
            Self::Caption => 13.,
        })
    }
    pub fn line_height(self) -> Pixels {
        px(match self {
            Self::Title => 36.,
            Self::Heading => 30.,
            Self::Body | Self::Label => 22.,
            Self::Caption => 19.,
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
