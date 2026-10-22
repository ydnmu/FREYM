use crate::ui::{
    primitives::{Button, ButtonSize, Icon, Icons, Text, TextVariant, clickable},
    theme::{colors::Color, spacing::Spacing},
};
use gpui::{
    AnyElement, App, ElementId, IntoElement, RenderOnce, SharedString, Window, div, prelude::*,
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureSourceKind {
    Display,
    Window,
    Application,
    Region,
    Camera,
}
impl CaptureSourceKind {
    fn icon(self) -> Icons {
        match self {
            Self::Display => Icons::Screen,
            Self::Window | Self::Application => Icons::Window,
            Self::Region => Icons::Area,
            Self::Camera => Icons::Camera,
        }
    }
}
#[derive(IntoElement)]
pub struct CaptureSourceCard {
    button: Button,
    title: SharedString,
    description: Option<SharedString>,
    kind: CaptureSourceKind,
    thumbnail: Option<AnyElement>,
}
impl CaptureSourceCard {
    pub fn new(id: impl Into<ElementId>, title: impl Into<SharedString>) -> Self {
        let title = title.into();
        Self {
            button: Button::new(id, title.clone())
                .size(ButtonSize::Row)
                .spotlight(),
            title,
            description: None,
            kind: CaptureSourceKind::Display,
            thumbnail: None,
        }
    }
    pub fn kind(mut self, kind: CaptureSourceKind) -> Self {
        self.kind = kind;
        self
    }
    pub fn description(mut self, description: impl Into<SharedString>) -> Self {
        self.description = Some(description.into());
        self
    }
    pub fn dimensions(self, width: u32, height: u32, refresh_hz: Option<f64>) -> Self {
        self.description(format!(
            "{width} × {height} · {}",
            refresh_hz
                .filter(|r| r.is_finite() && *r > 0.)
                .map(|r| format!("{r:.0} Hz"))
                .unwrap_or_else(|| "Refresh unknown".into())
        ))
    }
    pub fn thumbnail(mut self, thumbnail: impl IntoElement) -> Self {
        self.thumbnail = Some(thumbnail.into_any_element());
        self
    }
}
clickable!(CaptureSourceCard);
impl RenderOnce for CaptureSourceCard {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let selected =
            self.button.is_selected() && crate::ui::theme::ThemeId::current().is_motion();
        let mut labels = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(Spacing::Xxs.px())
            .child(Text::new(self.title).variant(TextVariant::Label).truncate());
        if let Some(description) = self.description {
            labels = labels.child(
                Text::new(description)
                    .variant(TextVariant::Caption)
                    .color(if selected {
                        Color::OnAccent
                    } else {
                        Color::SecondaryText
                    })
                    .truncate(),
            );
        }
        let mut row = div()
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap(Spacing::Md.px())
            .child(Icon::new(self.kind.icon()).color(if selected {
                Color::OnAccent
            } else {
                Color::Accent
            }));
        if let Some(thumbnail) = self.thumbnail {
            row = row.child(thumbnail);
        }
        self.button.content(row.child(labels))
    }
}
