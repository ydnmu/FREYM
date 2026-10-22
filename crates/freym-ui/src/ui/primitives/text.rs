use crate::ui::theme::{colors::Color, typography::Typography};
use gpui::{App, IntoElement, RenderOnce, SharedString, Window, div, prelude::*};
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TextVariant {
    Title,
    Heading,
    #[default]
    Body,
    Label,
    Muted,
    Caption,
}
#[derive(IntoElement)]
pub struct Text {
    value: SharedString,
    variant: TextVariant,
    color: Option<Color>,
    truncate: bool,
}
impl Text {
    pub fn new(value: impl Into<SharedString>) -> Self {
        Self {
            value: value.into(),
            variant: TextVariant::Body,
            color: None,
            truncate: false,
        }
    }
    pub fn variant(mut self, variant: TextVariant) -> Self {
        self.variant = variant;
        self
    }
    pub fn color(mut self, color: Color) -> Self {
        self.color = Some(color);
        self
    }
    pub fn truncate(mut self) -> Self {
        self.truncate = true;
        self
    }
}
impl RenderOnce for Text {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let settings = super::control_scope::current().settings;
        let style = match self.variant {
            TextVariant::Title => Typography::Title,
            TextVariant::Heading => Typography::Heading,
            TextVariant::Body => Typography::Body,
            TextVariant::Label => Typography::Label,
            TextVariant::Muted | TextVariant::Caption => Typography::Caption,
        };
        div()
            .min_w_0()
            .when(self.truncate, |d| d.truncate())
            .text_size(if settings {
                gpui::px(match self.variant {
                    TextVariant::Title => 20.,
                    TextVariant::Heading => 17.,
                    TextVariant::Body | TextVariant::Label => 12.,
                    TextVariant::Muted | TextVariant::Caption => 10.,
                })
            } else {
                style.size()
            })
            .font_weight(style.weight())
            .text_color(
                self.color
                    .unwrap_or(if self.variant == TextVariant::Muted {
                        Color::MutedText
                    } else {
                        Color::PrimaryText
                    })
                    .get(),
            )
            .child(self.value)
    }
}
