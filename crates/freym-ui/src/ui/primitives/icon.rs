//! One native vector vocabulary, shared stroke weight and viewbox.
use crate::ui::theme::{colors::Color, spacing::Layout};
use gpui::{AnimationExt, App, Hsla, IntoElement, RenderOnce, Svg, Window, prelude::*, px, svg};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icons {
    Screen,
    Window,
    Area,
    Camera,
    Mic,
    Audio,
    AudioMuted,
    Folder,
    Settings,
    Chevron,
    ArrowLeft,
    ChevronUp,
    ChevronDown,
    Close,
    Minimize,
    Maximize,
    Restore,
    Pause,
    Play,
    Refresh,
    Timer,
    Quality,
    Keyboard,
    Gauge,
    Palette,
    More,
    Plus,
    Grip,
    Visible,
    Hidden,
    Info,
}
fn icon(kind: Icons, color: Hsla) -> Svg {
    let path = match kind {
        Icons::Info => "<circle cx='12' cy='12' r='9'/><path d='M12 11v6m0-10h.01'/>",
        Icons::ChevronUp => "<path d='m6 15 6-6 6 6'/>",
        Icons::ChevronDown => "<path d='m6 9 6 6 6-6'/>",
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
        Icons::AudioMuted => "<path d='m11 4-6 5H2v6h3l6 5V4zm5 5 6 6m0-6-6 6'/>",
        Icons::Folder => "<path d='M3 6a2 2 0 0 1 2-2h4l2 3h8a2 2 0 0 1 2 2v10H3z'/>",
        Icons::Settings => {
            "<path d='M9.4 2.3h5.2l-.5 2 3.6 2 1.4-1.4 2.6 4.5-2 .6v4l2 .6-2.6 4.5-1.4-1.4-3.6 2 .5 2H9.4l.5-2-3.6-2-1.4 1.4L2.3 14.6l2-.6v-4l-2-.6 2.6-4.5 1.4 1.4 3.6-2z'/><circle cx='12' cy='12' r='3'/>"
        }
        Icons::Chevron => "<path d='m6 9 6 6 6-6'/>",
        Icons::ArrowLeft => "<path d='m10 5-7 7 7 7M3 12h18'/>",
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
        Icons::Keyboard => {
            "<rect x='2' y='5' width='20' height='14' rx='2'/><path d='M6 9h.01M10 9h.01M14 9h.01M18 9h.01M6 12h.01M10 12h.01M14 12h.01M18 12h.01M7 16h10'/>"
        }
        Icons::Gauge => {
            "<path d='M4.2 19a10 10 0 1 1 15.6 0M5 8l2 1m10-1 2-1M12 3v3m-4 14h8m-4-7 5-4'/><circle cx='12' cy='13' r='2'/>"
        }
        Icons::Palette => {
            "<path d='M12 3a9 9 0 1 0 0 18h1a2 2 0 0 0 1.4-3.4 1.5 1.5 0 0 1 1.1-2.6H18a3 3 0 0 0 3-3 9 9 0 0 0-9-9Z'/><circle cx='7.5' cy='10' r='.75'/><circle cx='10' cy='6.5' r='.75'/><circle cx='15' cy='7' r='.75'/><circle cx='17.5' cy='11' r='.75'/>"
        }
        Icons::More => {
            "<circle cx='5' cy='12' r='1'/><circle cx='12' cy='12' r='1'/><circle cx='19' cy='12' r='1'/>"
        }
        Icons::Plus => "<path d='M12 5v14M5 12h14'/>",
        Icons::Grip => {
            "<g fill='black' stroke='none'><circle cx='9' cy='6' r='1.25'/><circle cx='15' cy='6' r='1.25'/><circle cx='9' cy='12' r='1.25'/><circle cx='15' cy='12' r='1.25'/><circle cx='9' cy='18' r='1.25'/><circle cx='15' cy='18' r='1.25'/></g>"
        }
        Icons::Visible => {
            "<path d='M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7-10-7-10-7Z'/><circle cx='12' cy='12' r='3'/>"
        }
        Icons::Hidden => {
            "<path d='m3 3 18 18M10 5a12 12 0 0 1 12 7s-1.2 2.5-3.5 4.5M6.5 6.5A19 19 0 0 0 2 12s3.5 7 10 7a13 13 0 0 0 5.5-1.5'/>"
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
    color: Option<Color>,
    size: IconSize,
    rotation: Option<f32>,
}
impl Icon {
    pub fn new(kind: Icons) -> Self {
        Self {
            kind,
            color: Some(Color::SecondaryText),
            size: IconSize::Medium,
            rotation: None,
        }
    }
    pub fn color(mut self, color: Color) -> Self {
        self.color = Some(color);
        self
    }
    pub(crate) fn inherit_color(mut self) -> Self {
        self.color = None;
        self
    }
    pub fn size(mut self, size: IconSize) -> Self {
        self.size = size;
        self
    }
    pub fn rotation(mut self, degrees: f32) -> Self {
        self.rotation = Some(degrees);
        self
    }
}
impl RenderOnce for Icon {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let color = self
            .color
            .map(Color::get)
            .unwrap_or_else(|| _window.text_style().color);

        let image = icon(self.kind, color).size(px(match self.size {
            IconSize::Small => Layout::ICON_SMALL,
            IconSize::Medium => Layout::ICON,
            IconSize::Large => Layout::ICON_LARGE,
        }));

        let image = if let Some(rotation) = self.rotation {
            image
                .with_spring(
                    "icon-rotation",
                    crate::ui::theme::motion::selection(rotation),
                    |image, degrees| {
                        image.with_transformation(gpui::Transformation::rotate(gpui::Radians(
                            degrees.to_radians(),
                        )))
                    },
                )
                .into_any_element()
        } else {
            image.into_any_element()
        };

        image.into_any_element()
    }
}
