use crate::ui::theme::{Theme, colors::Color, radius::Radius, spacing::Spacing};
use gpui::{AnyElement, App, IntoElement, ParentElement, RenderOnce, Window, div, prelude::*};
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub enum SurfaceVariant {
    #[default]
    Base,
    Elevated,
    Interactive,
}
#[derive(IntoElement)]
pub struct Surface {
    variant: SurfaceVariant,
    children: Vec<AnyElement>,
}
impl Surface {
    pub fn new() -> Self {
        Self {
            variant: SurfaceVariant::Base,
            children: vec![],
        }
    }
    pub fn variant(mut self, variant: SurfaceVariant) -> Self {
        self.variant = variant;
        self
    }
}
impl Default for Surface {
    fn default() -> Self {
        Self::new()
    }
}
impl ParentElement for Surface {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}
impl RenderOnce for Surface {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let settings = super::control_scope::current().settings;
        div()
            .min_w_0()
            .p(Spacing::Md.px())
            .rounded(Radius::Large.px())
            .border_1()
            .border_color(Color::SubtleBorder.get())
            .bg(if self.variant == SurfaceVariant::Elevated {
                Color::ElevatedSurface.get()
            } else {
                Color::PrimarySurface.get()
            })
            .when(self.variant == SurfaceVariant::Elevated, |d| {
                d.shadow(vec![Theme::shadow()])
            })
            .when(self.variant == SurfaceVariant::Interactive, |d| {
                d.hover(|s| s.bg(Color::HoverSurface.get()))
            })
            .map(|d| {
                if settings {
                    d.p(gpui::px(0.))
                        .rounded(gpui::px(0.))
                        .border_color(Color::SubtleBorder.get().opacity(0.))
                        .bg(Color::PrimarySurface.get().opacity(0.))
                        .shadow(vec![])
                } else if crate::ui::theme::ThemeId::current().is_motion() {
                    crate::ui::theme::material::Material::Graphite
                        .apply(d, crate::ui::theme::low_cost_effects())
                } else {
                    d
                }
            })
            .children(self.children)
    }
}
