//! One native vector vocabulary, shared stroke weight and viewbox.
//! Stroke paths are derived from Lucide (ISC); see NOTICE and LICENSES/Lucide.txt.
use crate::ui::theme::{colors::Color, spacing::Layout};
use gpui::{App, Hsla, IntoElement, RenderOnce, Svg, Window, prelude::*, px, svg};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icons {
    Screen,
    Window,
    Area,
    Camera,
    Mic,
    Audio,
    Folder,
    Settings,
    Chevron,
    Close,
    Minimize,
    Maximize,
    Restore,
    Pause,
    Play,
    Refresh,
    Timer,
    Quality,
    More,
}
fn icon(kind: Icons, color: Hsla) -> Svg {
    let path = match kind {
        Icons::Screen => {
            "<rect x='3' y='4' width='18' height='13' rx='2'/><path d='M8 21h8m-4-4v4'/>"
        }
        Icons::Window => {
            "<rect x='3' y='4' width='18' height='16' rx='2'/><path d='M3 9h18m-14-3h.01M10 6h.01'/>"
        }
        Icons::Area => "<path d='M8 3H3v5m13-5h5v5M3 16v5h5m8 0h5v-5M8 8h8v8H8z'/>",
        Icons::Camera => {
            "<rect x='3' y='6' width='12' height='12' rx='2'/><path d='m15 10 6-3v10l-6-3'/>"
        }
        Icons::Mic => {
            "<rect x='9' y='2' width='6' height='13' rx='3'/><path d='M5 10v2a7 7 0 0 0 14 0v-2m-7 9v3m-4 0h8'/>"
        }
        Icons::Audio => {
            "<path d='m11 4-6 5H2v6h3l6 5V4zm4 4a6 6 0 0 1 0 8m3-11a10 10 0 0 1 0 14'/>"
        }
        Icons::Folder => "<path d='M3 6a2 2 0 0 1 2-2h4l2 3h8a2 2 0 0 1 2 2v10H3z'/>",
        Icons::Settings => {
            "<path d='M9.4 2.3h5.2l-.5 2 3.6 2 1.4-1.4 2.6 4.5-2 .6v4l2 .6-2.6 4.5-1.4-1.4-3.6 2 .5 2H9.4l.5-2-3.6-2-1.4 1.4L2.3 14.6l2-.6v-4l-2-.6 2.6-4.5 1.4 1.4 3.6-2z'/><circle cx='12' cy='12' r='3'/>"
        }
        Icons::Chevron => "<path d='m6 9 6 6 6-6'/>",
        Icons::Close => "<path d='m6 6 12 12M6 18 18 6'/>",
        Icons::Minimize => "<path d='M5 12h14'/>",
        Icons::Maximize => "<rect x='6' y='6' width='12' height='12'/>",
        Icons::Restore => "<path d='M9 6V3h12v12h-3'/><rect x='3' y='9' width='12' height='12'/>",
        Icons::Pause => "<path d='M8 5v14M16 5v14'/>",
        Icons::Play => "<path d='m7 4 14 8-14 8z'/>",
        Icons::Refresh => "<path d='M20 8a8 8 0 1 0 0 8m0-13v5h-5'/>",
        Icons::Timer => {
            "<circle cx='12' cy='14' r='8'/><path d='M9 2h6m-3 0v4m0 4v4l3 2m3-11 2 2'/>"
        }
        Icons::Quality => {
            "<path d='m12 3 2.8 5.7 6.2.9-4.5 4.4 1.1 6.2-5.6-3-5.6 3 1.1-6.2L3 9.6l6.2-.9z'/>"
        }
        Icons::More => {
            "<circle cx='5' cy='12' r='1'/><circle cx='12' cy='12' r='1'/><circle cx='19' cy='12' r='1'/>"
        }
    };
    let data = format!(
        "<svg xmlns='http://www.w3.org/2000/svg' width='24' height='24' viewBox='0 0 24 24' fill='none' stroke='black' stroke-width='1.5' stroke-linecap='round' stroke-linejoin='round'>{path}</svg>"
    );
    svg()
        .data(data.as_bytes())
        .size(px(Layout::ICON))
        .text_color(color)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IconSize {
    Small,
    Medium,
    Large,
}
#[derive(IntoElement)]
pub struct Icon {
    kind: Icons,
    color: Color,
    size: IconSize,
    rotation: f32,
}
impl Icon {
    pub fn new(kind: Icons) -> Self {
        Self {
            kind,
            color: Color::SecondaryText,
            size: IconSize::Medium,
            rotation: 0.,
        }
    }
    pub fn color(mut self, color: Color) -> Self {
        self.color = color;
        self
    }
    pub fn size(mut self, size: IconSize) -> Self {
        self.size = size;
        self
    }
    pub fn rotation(mut self, degrees: f32) -> Self {
        self.rotation = degrees;
        self
    }
}
impl RenderOnce for Icon {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        icon(self.kind, self.color.get())
            .with_transformation(gpui::Transformation::rotate(gpui::Radians(
                self.rotation.to_radians(),
            )))
            .size(px(match self.size {
                IconSize::Small => Layout::ICON_SMALL,
                IconSize::Medium => Layout::ICON,
                IconSize::Large => Layout::ICON_LARGE,
            }))
    }
}
