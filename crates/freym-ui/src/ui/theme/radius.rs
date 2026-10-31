use gpui::{Pixels, px};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Radius {
    Small,
    Medium,
    Large,
}
impl Radius {
    pub const fn value(self) -> f32 {
        match self {
            Self::Small => 8.,
            Self::Medium => 9.,
            Self::Large => 12.,
        }
    }
    pub fn px(self) -> Pixels {
        px(self.value())
    }
}
