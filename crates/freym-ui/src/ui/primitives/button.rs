use super::{Icon, Icons};
use crate::ui::theme::{
    colors::Color,
    radius::Radius,
    spacing::{Layout, Spacing},
    typography::Typography,
};
use gpui::{
    AnimationExt, AnyElement, App, ClickEvent, ElementId, IntoElement, RenderOnce, SharedString,
    Window, div, prelude::*, px,
};
use std::rc::Rc;
pub(crate) type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ButtonVariant {
    Primary,
    #[default]
    Secondary,
    Ghost,
    Danger,
    Record,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ButtonSize {
    #[default]
    Regular,
    Compact,
    Icon,
    Dock,
    Nav,
    Record,
    Row,
    Card,
}

#[derive(IntoElement)]
pub struct Button {
    id: ElementId,
    label: SharedString,
    icon: Option<Icons>,
    variant: ButtonVariant,
    size: ButtonSize,
    selected: bool,
    disabled: bool,
    handler: Option<ClickHandler>,
    content: Vec<AnyElement>,
    control_area: Option<gpui::WindowControlArea>,
    toggled: Option<bool>,
    spotlight: bool,
    segment: bool,
    expanded: Option<bool>,
    focus: Option<gpui::FocusHandle>,
}
impl Button {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon: None,
            variant: ButtonVariant::Secondary,
            size: ButtonSize::Regular,
            selected: false,
            disabled: false,
            handler: None,
            content: vec![],
            control_area: None,
            toggled: None,
            spotlight: false,
            segment: false,
            expanded: None,
            focus: None,
        }
    }
    pub fn icon(mut self, icon: Icons) -> Self {
        self.icon = Some(icon);
        self
    }
    pub(crate) fn focus(mut self, focus: gpui::FocusHandle) -> Self {
        self.focus = Some(focus);
        self
    }
    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = label.into();
        self
    }
    pub fn variant(mut self, variant: ButtonVariant) -> Self {
        self.variant = variant;
        self
    }
    pub fn size(mut self, size: ButtonSize) -> Self {
        self.size = size;
        self
    }
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
    pub fn expanded(mut self, expanded: bool) -> Self {
        self.expanded = Some(expanded);
        self
    }
    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.handler = Some(Rc::new(handler));
        self
    }
    pub(crate) fn content(mut self, content: impl IntoElement) -> Self {
        self.content.push(content.into_any_element());
        self
    }
    pub(crate) fn window_control(mut self, area: gpui::WindowControlArea) -> Self {
        self.control_area = Some(area);
        self
    }
    pub(crate) fn toggled(mut self, checked: bool) -> Self {
        self.toggled = Some(checked);
        self
    }
    pub(crate) fn id(&self) -> &ElementId {
        &self.id
    }
    pub fn is_disabled(&self) -> bool {
        self.disabled
    }
    pub fn is_selected(&self) -> bool {
        self.selected
    }
    pub(crate) fn spotlight(mut self) -> Self {
        self.spotlight = true;
        self
    }
    pub(crate) fn segment(mut self) -> Self {
        self.segment = true;
        self
    }
}
impl RenderOnce for Button {
    fn render(mut self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        use crate::ui::theme::controls::{self, Phase, Role};
        let context = super::control_scope::current();
        self.disabled |= context.inert;
        let role = if self.toggled.is_some() || self.control_area.is_some() || self.segment {
            Role::Quiet
        } else if self.selected || self.expanded == Some(true) {
            Role::Selection
        } else {
            match self.variant {
                ButtonVariant::Primary => Role::Primary,
                ButtonVariant::Record => Role::Record,
                ButtonVariant::Ghost => Role::Quiet,
                ButtonVariant::Danger => Role::Danger,
                ButtonVariant::Secondary => Role::Secondary,
            }
        };
        let phase = if self.disabled {
            Phase::Disabled
        } else if self.selected || self.expanded == Some(true) {
            Phase::Selected
        } else {
            Phase::Rest
        };
        let motion = crate::ui::theme::ThemeId::current().is_motion();
        let foreground = match self.variant {
            _ if motion && self.selected => Color::OnAccent,
            ButtonVariant::Primary | ButtonVariant::Record => Color::OnAccent,
            ButtonVariant::Danger => Color::Error,
            _ if self.selected || self.size == ButtonSize::Record => Color::PrimaryText,
            _ => Color::SecondaryText,
        };
        let background = match self.variant {
            ButtonVariant::Primary => Color::Accent.get(),
            ButtonVariant::Record => Color::Record.get(),
            ButtonVariant::Secondary => Color::PrimarySurface.get(),
            ButtonVariant::Danger | ButtonVariant::Ghost if self.selected => {
                Color::PrimarySurface.get()
            }
            _ => Color::Background.get().opacity(0.),
        };
        let debug = format!("{}", self.id);
        let press_id = (!self.disabled && self.control_area.is_none() && !cx.reduce_motion())
            .then(|| format!("motion-press-{}", self.id));
        let light = (self.spotlight && !self.disabled && !motion).then(|| {
            window.use_keyed_state(format!("light-{}", self.id), cx, |_, _| {
                super::card_light::CardLight::default()
            })
        });
        let mut element = div()
            .id(self.id)
            .debug_selector(move || debug.clone())
            .role(gpui::Role::Button)
            .aria_label(self.label.clone())
            .aria_selected(self.selected)
            .when_some(self.focus, |d, focus| d.track_focus(&focus))
            .when_some(self.expanded, |d, expanded| d.aria_expanded(expanded))
            .flex()
            .items_center()
            .justify_center()
            .gap(Spacing::Xs.px())
            .min_w_0()
            .flex_shrink_0()
            .rounded(Radius::Medium.px())
            .border_1()
            .border_color(if self.selected {
                Color::ActiveBorder.get().opacity(0.35)
            } else if self.variant == ButtonVariant::Secondary {
                Color::DefaultBorder.get()
            } else {
                Color::DefaultBorder.get().opacity(0.)
            })
            .font_family(crate::ui::theme::typography::FONT_FAMILY)
            .text_size(px(if context.settings { 12. } else { 13. }))
            .line_height(px(if context.settings { 17.4 } else { 18.85 }))
            .font_weight(Typography::Label.weight())
            .text_color(foreground.get())
            .bg(background)
            .opacity(if self.disabled { 0.55 } else { 1. });
        if motion && self.selected && !self.segment {
            element = element
                .bg(Color::Accent.get())
                .text_color(Color::OnAccent.get());
        }
        if self.size == ButtonSize::Record && self.variant != ButtonVariant::Record {
            element = element
                .bg(gpui::rgb(0xf5f5f5))
                .text_color(gpui::rgb(0x111111));
        }
        if self.segment {
            element = element
                .w_full()
                .bg(Color::Background.get().opacity(0.))
                .border_color(Color::DefaultBorder.get().opacity(0.));
        }
        element = match self.size {
            ButtonSize::Regular => element
                .h(px(if context.settings {
                    30.
                } else {
                    Layout::CONTROL_HEIGHT
                }))
                .px(px(12.)),
            ButtonSize::Compact => element
                .h(px(if context.settings {
                    30.
                } else {
                    Layout::COMPACT_CONTROL
                }))
                .px(px(12.)),
            ButtonSize::Icon => element.size(px(if context.settings {
                30.
            } else {
                Layout::ICON_CONTROL
            })),
            ButtonSize::Dock => element
                .w(px(
                    if self.label.as_ref() == "Studio"
                        && f32::from(window.viewport_size().width) >= 1400.
                    {
                        106.
                    } else {
                        54.
                    },
                ))
                .h(px(54.)),
            ButtonSize::Nav => element.w_full().h(px(36.)).px(px(12.)).justify_start(),
            ButtonSize::Record => element
                .w_full()
                .h(px(Layout::RECORD_HEIGHT))
                .text_size(px(16.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .rounded(px(8.)),
            ButtonSize::Row => element
                .w_full()
                .px(px(12.))
                .py(px(if context.settings { 8. } else { 7. }))
                .justify_start(),
            ButtonSize::Card => element
                .w_full()
                .h_full()
                .px(Spacing::Md.px())
                .justify_start(),
        };
        element = controls::apply(element, role, phase, context.settings).rounded(px(
            if crate::ui::theme::filled_buttons() || context.settings {
                8.
            } else {
                9.
            },
        ));
        if self.size == ButtonSize::Nav {
            element = element
                .rounded(px(5.))
                .border_color(Color::DefaultBorder.get().opacity(0.));
        }
        if !self.disabled {
            let segment = self.segment;
            let spotlight = self.spotlight;
            element = element
                .cursor_pointer()
                .focusable()
                .tab_index(0)
                .hover(move |s| {
                    let mut s = controls::apply(s, role, Phase::Hover, context.settings);
                    if segment {
                        s = s.bg(Color::Accent.get().opacity(0.04));
                    }
                    if spotlight {
                        s.border_color(Color::ActiveBorder.get().opacity(0.28))
                    } else {
                        s
                    }
                })
                .active(move |s| controls::apply(s, role, Phase::Pressed, context.settings))
                .focus_visible(|s| s.border_color(Color::ActiveBorder.get()));
            if let Some(light) = light {
                let hover_light = light.clone();
                let move_light = light.clone();
                element = element
                    .relative()
                    .overflow_hidden()
                    .on_hover(move |hovered, w, cx| {
                        hover_light.update(cx, |light, cx| {
                            if *hovered {
                                light.move_to(w.mouse_position(), cx);
                            } else {
                                light.leave(cx);
                            }
                        })
                    })
                    .on_mouse_move(move |event, _, cx| {
                        move_light.update(cx, |light, cx| light.move_to(event.position, cx))
                    })
                    .child(div().absolute().top_0().left_0().size_full().child(light));
            }
            if let Some(handler) = self.handler {
                element = element.on_click(move |e, w, cx| {
                    cx.stop_propagation();
                    handler(e, w, cx);
                });
            }
        }
        if let Some(area) = self.control_area {
            element = element.window_control_area(area);
        }
        if let Some(checked) = self.toggled {
            element = element.role(gpui::Role::Switch).aria_toggled(if checked {
                gpui::Toggled::True
            } else {
                gpui::Toggled::False
            });
        }
        if self.disabled {
            element = element.aria_description("Unavailable");
        }
        if let Some(icon) = self.icon {
            element = element.child(Icon::new(icon).color(foreground));
        }
        if self.content.is_empty() {
            if self.size != ButtonSize::Icon
                && (self.size != ButtonSize::Dock
                    || (self.label.as_ref() == "Studio"
                        && f32::from(window.viewport_size().width) >= 1400.))
            {
                element = element.child(self.label);
            }
        } else {
            element = element.children(self.content);
        }
        if let Some(press_id) = press_id {
            let state = window
                .use_keyed_state(press_id, cx, |_, _| super::motion_press::MotionPress::new());
            let pressed = state.read(cx).pressed;
            let down = state.clone();
            let up = state.clone();
            let out = state.clone();
            let leave = state.clone();
            let key_down = state.clone();
            element = element
                .on_mouse_down(gpui::MouseButton::Left, move |_, _, cx| {
                    down.update(cx, |s, cx| s.set(true, cx))
                })
                .on_mouse_up(gpui::MouseButton::Left, move |_, _, cx| {
                    up.update(cx, |s, cx| s.set(false, cx))
                })
                .on_mouse_up_out(gpui::MouseButton::Left, move |_, _, cx| {
                    out.update(cx, |s, cx| s.set(false, cx))
                })
                .on_hover(move |hovered, _, cx| {
                    if !hovered {
                        leave.update(cx, |s, cx| s.set(false, cx));
                    }
                })
                .on_key_down(move |e, _, cx| {
                    if matches!(e.keystroke.key.as_str(), "space" | "enter") {
                        key_down.update(cx, |s, cx| s.set(true, cx));
                    }
                })
                .on_key_up(move |_, _, cx| state.update(cx, |s, cx| s.set(false, cx)));
            return element
                .with_spring(
                    "motion-compression",
                    crate::ui::theme::motion::press(if pressed { 1. } else { 0. }),
                    move |d, t| d.relative().top(px(t.clamp(0., 1.))),
                )
                .into_any_element();
        }
        element.into_any_element()
    }
}
