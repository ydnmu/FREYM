use super::{Icon, Icons};
use crate::ui::theme::{
    colors::Color,
    radius::Radius,
    spacing::{Layout, Spacing},
    typography::Typography,
};
use gpui::{
    AnyElement, App, ClickEvent, ElementId, IntoElement, RenderOnce, SharedString, Window, div,
    prelude::*, px,
};
use std::rc::Rc;
pub(crate) type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

struct ButtonTooltip(SharedString);
impl gpui::Render for ButtonTooltip {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
            .max_w(px(360.))
            .p(px(8.))
            .rounded(px(6.))
            .bg(crate::ui::theme::surface_treatment::overlay_color())
            .text_color(Color::PrimaryText.get())
            .border_1()
            .border_color(Color::DefaultBorder.get())
            .text_size(px(12.))
            .line_height(px(16.))
            .child(self.0.clone())
    }
}
struct TooltipState {
    focus: gpui::FocusHandle,
    bounds: Option<gpui::Bounds<gpui::Pixels>>,
    dismissed: bool,
}
fn with_tooltip(
    element: gpui::Stateful<gpui::Div>,
    label: SharedString,
    state: gpui::Entity<TooltipState>,
    window: &mut Window,
    cx: &mut App,
) -> gpui::Stateful<gpui::Div> {
    let focused = state.read(cx).focus.is_focused(window);
    if !focused {
        state.update(cx, |s, _| s.dismissed = false);
    }
    let measure = state.clone();
    let dismiss = state.clone();
    let hover_label = label.clone();
    let mut element = element
        .tooltip(move |_, cx| cx.new(|_| ButtonTooltip(hover_label.clone())).into())
        .on_key_down(move |event, _, cx| {
            if event.keystroke.key == "escape" {
                dismiss.update(cx, |s, cx| {
                    s.dismissed = true;
                    cx.notify();
                });
            }
        })
        .child(
            gpui::canvas(
                move |bounds, _, cx| measure.update(cx, |s, _| s.bounds = Some(bounds)),
                |_, _, _, _| {},
            )
            .absolute()
            .inset_0()
            .size_full(),
        );
    if focused
        && window.last_input_was_keyboard()
        && !state.read(cx).dismissed
        && let Some(bounds) = state.read(cx).bounds
    {
        // Deferred anchoring escapes the panel's scroll clip and stays in the window.
        element = element.child(
            gpui::deferred(
                gpui::anchored()
                    .position(gpui::point(bounds.left(), bounds.bottom() + px(6.)))
                    .snap_to_window_with_margin(px(8.))
                    .child(
                        div()
                            .id("control-focus-tooltip")
                            .debug_selector(|| "control-focus-tooltip".into())
                            .max_w(px(240.))
                            .px(px(8.))
                            .py(px(6.))
                            .rounded(px(6.))
                            .bg(crate::ui::theme::surface_treatment::overlay_color())
                            .border_1()
                            .border_color(Color::DefaultBorder.get())
                            .text_size(px(12.))
                            .line_height(px(16.))
                            .text_color(Color::PrimaryText.get())
                            .child(label),
                    ),
            )
            .with_priority(4),
        );
    }
    element
}

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
    Small,
    Icon,
    Dock,
    Nav,
    Record,
    RecordCompact,
    Row,
    ListRow,
    ListIcon,
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
    fill_width: bool,
    tooltip: Option<SharedString>,
    deck_control: bool,
    keycap: bool,
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
            fill_width: false,
            tooltip: None,
            deck_control: false,
            keycap: false,
        }
    }
    pub fn icon(mut self, icon: Icons) -> Self {
        self.icon = Some(icon);
        self
    }
    pub(crate) fn tooltip(mut self, label: impl Into<SharedString>) -> Self {
        self.tooltip = Some(label.into());
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
    pub(crate) fn deck_control(mut self) -> Self {
        self.deck_control = true;
        self
    }
    pub(crate) fn fill_width(mut self) -> Self {
        self.fill_width = true;
        self
    }
    pub(crate) fn keycap(mut self) -> Self {
        self.keycap = true;
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

        let studying = false;

        let context = super::control_scope::current();
        let home = context.home && !context.settings;
        let nav = self.size == ButtonSize::Nav;
        let deck = self.deck_control;
        let list = matches!(self.size, ButtonSize::ListRow | ButtonSize::ListIcon);
        let segment = self.segment && context.segment_indicator;
        let window_control = self.control_area.is_some();
        let danger_control = self.variant == ButtonVariant::Danger;
        let raised = !window_control
            && !deck
            && !segment
            && !list
            && !nav
            && (home || context.settings || crate::ui::theme::filled_buttons());
        let flat = list || nav;
        let viewport = crate::ui::viewport(window, cx);
        let geometry = crate::ui::components::RecorderGeometry::new(
            viewport.width.into(),
            viewport.height.into(),
        );
        let smooth_fill =
            studying || (!context.settings && (nav || (raised && !flat)) && !cx.reduce_motion());
        self.disabled |= context.inert;
        let role = if self.keycap && context.settings {
            Role::Keycap
        } else if list || self.toggled.is_some() || self.control_area.is_some() || self.segment {
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
        let debug = format!("{}", self.id);
        let response_id = format!("control-response-{}", self.id);

        let design_editing = false;
        let press_id = (studying
            || (!context.settings
                && !segment
                && !deck
                && !list
                && !self.disabled
                && self.control_area.is_none()
                && !cx.reduce_motion()))
        .then(|| format!("motion-press-{}", self.id));
        let light = (self.spotlight && !list && !self.disabled).then(|| {
            window.use_keyed_state(format!("light-{}", self.id), cx, |_, _| {
                super::card_light::CardLight::default()
            })
        });
        let tooltip_label = self.tooltip.clone().or_else(|| {
            (home
                && matches!(
                    self.size,
                    ButtonSize::Icon | ButtonSize::ListIcon | ButtonSize::Dock
                ))
            .then(|| self.label.clone())
        });
        let tooltip = tooltip_label
            .filter(|_| !studying && !design_editing)
            .map(|label| {
                let focus = self.focus.clone();
                let state =
                    window.use_keyed_state(format!("control-tooltip-{}", self.id), cx, |_, cx| {
                        TooltipState {
                            focus: focus.unwrap_or_else(|| cx.focus_handle().tab_stop(true)),
                            bounds: None,
                            dismissed: false,
                        }
                    });
                self.focus = Some(state.read(cx).focus.clone());
                (label, state)
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
            .text_size(Typography::Label.size())
            .line_height(Typography::Label.line_height())
            .font_weight(Typography::Label.weight())
            .opacity(if self.disabled { 0.55 } else { 1. });
        if self.segment {
            element = element
                .w_full()
                .border_color(Color::DefaultBorder.get().opacity(0.));
        }
        element = match self.size {
            ButtonSize::Regular => element
                .min_h(px(Layout::CONTROL_HEIGHT))
                .when(!context.settings, |d| d.h(px(Layout::CONTROL_HEIGHT)))
                .when(context.settings, |d| d.py(px(8.)).whitespace_normal())
                .px(px(16.)),
            ButtonSize::Compact => element
                .min_h(px(if context.settings {
                    34.
                } else {
                    Layout::COMPACT_CONTROL
                }))
                .when(!context.settings, |d| d.h(px(Layout::COMPACT_CONTROL)))
                .when(context.settings, |d| d.py(px(7.)).whitespace_normal())
                .px(px(12.)),
            ButtonSize::Small => element
                .h(px(32.))
                .px(px(8.))
                .gap(px(4.))
                .text_size(px(12.))
                .line_height(px(16.)),
            ButtonSize::Icon => element.size(px(Layout::ICON_CONTROL)),
            ButtonSize::Dock => element
                .when(home, |d| {
                    d.h(px(geometry.transport_height()))
                        .w(px(geometry.dock_width(false)))
                        .flex_shrink_0()
                })
                .when(!home, |d| d.h(px(54.)).w(px(54.))),
            ButtonSize::Nav => element
                .w_full()
                .h(px(44.))
                .px(px(12.))
                .gap(px(12.))
                .when(self.selected, |d| d.font_weight(gpui::FontWeight::SEMIBOLD))
                .justify_start(),
            ButtonSize::Record | ButtonSize::RecordCompact => element
                .w_full()
                .h(px(if self.size == ButtonSize::RecordCompact {
                    42.
                } else if home {
                    geometry.transport_height()
                } else {
                    Layout::RECORD_HEIGHT
                }))
                .text_size(px(if self.size == ButtonSize::RecordCompact {
                    14.
                } else if home {
                    18.
                } else {
                    16.
                }))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .rounded(px(8.)),
            ButtonSize::Row => element
                .w_full()
                .px(px(12.))
                .py(px(if context.settings { 8. } else { 7. }))
                .justify_start(),
            ButtonSize::ListRow => element.w_full().h(px(36.)).px(px(8.)).justify_start(),
            ButtonSize::ListIcon => element.size(px(28.)),
            ButtonSize::Card => element
                .w_full()
                .h_full()
                .px(Spacing::Md.px())
                .justify_start(),
        };
        element = element.when(self.fill_width, |d| d.w_full());
        element = element.when(context.settings && !nav && !list, |d| {
            d.flex_shrink_1().max_w_full()
        });
        element = if window_control {
            controls::window_control(element, phase, danger_control)
        } else if segment {
            controls::segment(element, phase)
        } else if nav {
            controls::navigation(element, phase, 0., 0.)
        } else {
            controls::apply(element, role, phase, flat)
        }
        .rounded(px(
            if crate::ui::theme::filled_buttons() || context.settings {
                8.
            } else {
                9.
            },
        ));
        if self.size == ButtonSize::Nav {
            element = element
                .rounded(px(6.))
                .border_color(Color::DefaultBorder.get().opacity(0.));
        }
        if list {
            element = element
                .border_0()
                .rounded(px(6.))
                .text_size(px(13.))
                .line_height(px(18.))
                .font_weight(gpui::FontWeight::MEDIUM)
                .when(self.disabled, |d| d.bg(Color::Background.get().opacity(0.)));
        }
        if raised && !flat {
            element = controls::raised(element, role, phase, 0., 0.).rounded(px(10.));
        }
        if deck {
            element = controls::deck_surface(element.size(px(42.)).rounded(px(12.)), phase);
        }
        if !self.disabled {
            let segment = self.segment && !segment;
            let indicator_owned = self.segment && context.segment_indicator;
            let spotlight = self.spotlight;
            element = element
                .cursor_pointer()
                .focusable()
                .tab_index(0)
                .hover(move |s| {
                    if deck {
                        return controls::deck_surface(s, Phase::Hover);
                    }
                    if window_control {
                        return controls::window_control(s, Phase::Hover, danger_control);
                    }
                    if indicator_owned {
                        return controls::segment(s, Phase::Hover);
                    }
                    if smooth_fill {
                        return s;
                    }
                    let mut s = if nav {
                        controls::navigation(s, phase, 1., 0.)
                    } else if raised && !flat {
                        controls::raised(s, role, Phase::Hover, 1., 0.)
                    } else {
                        controls::apply(s, role, Phase::Hover, flat)
                    };
                    if segment {
                        s = s.bg(Color::Accent.get().opacity(0.04));
                    }
                    if spotlight {
                        s.border_color(Color::ActiveBorder.get().opacity(0.28))
                    } else {
                        s
                    }
                })
                .active(move |s| {
                    if deck {
                        return controls::deck_surface(s, Phase::Pressed);
                    }
                    if window_control {
                        return controls::window_control(s, Phase::Pressed, danger_control);
                    }
                    if indicator_owned {
                        return controls::segment(s, Phase::Pressed);
                    }
                    if smooth_fill {
                        s
                    } else {
                        if nav {
                            controls::navigation(s, phase, 1., 1.)
                        } else if raised && !flat {
                            controls::raised(s, role, Phase::Pressed, 1., 1.)
                        } else {
                            controls::apply(s, role, Phase::Pressed, flat)
                        }
                    }
                })
                .focus_visible(move |s| {
                    if deck {
                        controls::deck_surface(s, Phase::Hover)
                            .border_color(Color::ActiveBorder.get())
                    } else if window_control {
                        controls::window_control(s, Phase::Hover, danger_control)
                            .text_color(Color::ActiveBorder.get())
                    } else if indicator_owned {
                        controls::segment(s, phase).text_color(Color::ActiveBorder.get())
                    } else if nav {
                        controls::navigation(s, phase, 1., 0.)
                    } else if list {
                        s.bg(Color::HoverSurface.get())
                    } else if !crate::ui::theme::filled_buttons() {
                        s.text_color(Color::ActiveBorder.get()).underline()
                    } else {
                        s.border_color(Color::ActiveBorder.get())
                    }
                });
            if let Some(light) = light.as_ref() {
                let move_light = light.clone();
                element = element
                    .relative()
                    .overflow_hidden()
                    .on_mouse_move(move |event, _, cx| {
                        move_light.update(cx, |light, cx| light.move_to(event.position, cx))
                    })
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full()
                            .child(light.clone()),
                    );
            }
            if let Some(handler) = self.handler.filter(|_| !design_editing) {
                element = element.on_click(move |e, w, cx| {
                    cx.stop_propagation();
                    handler(e, w, cx);
                });
            }
        }
        if let Some(area) = self.control_area.filter(|_| !design_editing) {
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
            element = element.tab_stop(false).aria_description("Unavailable");
        }
        if let Some((label, state)) = tooltip {
            element = with_tooltip(element, label, state, window, cx);
        }
        let mut content = Vec::<AnyElement>::new();
        if let Some(icon) = self.icon.filter(|_| !studying) {
            content.push(Icon::new(icon).inherit_color().into_any_element());
        }
        if self.content.is_empty() {
            if !matches!(self.size, ButtonSize::Icon | ButtonSize::ListIcon)
                && self.size != ButtonSize::Dock
            {
                if studying {
                    content.push(
                        div()
                            .min_w_0()
                            .text_ellipsis()
                            .child(self.label.clone())
                            .into_any_element(),
                    );
                } else {
                    content.push(self.label.clone().into_any_element());
                }
            }
        } else {
            content.extend(self.content);
        }
        // GPUI supports exactly one hover listener per element. Light and press
        // effects must share it, including when reduced motion omits press state.
        let has_light = light.is_some();
        let hover_light = move |hovered: &bool, window: &mut Window, cx: &mut App| {
            if let Some(light) = light.as_ref() {
                light.update(cx, |light, cx| {
                    if *hovered {
                        light.move_to(window.mouse_position(), cx);
                    } else {
                        light.leave(cx);
                    }
                });
            }
        };

        if let Some(press_id) = press_id {
            let state = window.use_keyed_state(press_id, cx, |window, cx| {
                #[allow(unused_mut)]
                let mut state = super::motion_press::MotionPress::new();

                let _ = (window, cx);
                state
            });
            let pressed = state.read(cx).pressed;
            use crate::ui::theme::motion;
            let hover_config = motion::response_spring(motion::HOVER_MS as f32);

            let press_config = motion::response_spring(if pressed {
                motion::PRESS_RESPONSE_MS
            } else {
                motion::RELEASE_RESPONSE_MS
            });

            let response = state.update(cx, |state, _| state.sample(hover_config, press_config));
            let down = state.clone();
            let up = state.clone();
            let out = state.clone();
            let leave = state.clone();
            let key_down = state.clone();
            element = element.when(!self.disabled, |element| {
                element
                    .on_mouse_down(gpui::MouseButton::Left, move |_, _, cx| {
                        down.update(cx, |s, cx| s.set(true, cx))
                    })
                    .on_mouse_up(gpui::MouseButton::Left, move |_, _, cx| {
                        up.update(cx, |s, cx| s.set(false, cx))
                    })
                    .on_mouse_up_out(gpui::MouseButton::Left, move |_, _, cx| {
                        out.update(cx, |s, cx| s.set(false, cx))
                    })
                    .on_hover(move |hovered, window, cx| {
                        hover_light(hovered, window, cx);
                        if raised || nav || studying {
                            leave.update(cx, |s, cx| {
                                if nav || context.settings {
                                    s.nav_hover(*hovered, cx)
                                } else {
                                    s.hover(*hovered, cx)
                                }
                            });
                        }
                        if !hovered && !studying {
                            leave.update(cx, |s, cx| s.set(false, cx));
                        }
                    })
                    .on_key_down(move |e, _, cx| {
                        if matches!(e.keystroke.key.as_str(), "space" | "enter")
                            && (!studying || !e.keystroke.modifiers.modified())
                        {
                            key_down.update(cx, |s, cx| s.set(true, cx));
                        }
                    })
                    .on_key_up({
                        let state = state.clone();
                        move |_, _, cx| state.update(cx, |s, cx| s.set(false, cx))
                    })
            });

            if response.2 {
                window.request_animation_frame();
            }
            // Press feedback changes material only; face, content and hit area stay fixed.
            let content_id = format!("press-content-{response_id}");
            let face = div()
                .id(gpui::SharedString::from(content_id.clone()))
                .debug_selector(move || content_id.clone())
                .flex()
                .flex_1()
                .self_stretch()
                .min_w_0()
                .max_w_full()
                .items_center()
                .justify_center()
                .when(
                    matches!(
                        self.size,
                        ButtonSize::Nav | ButtonSize::Row | ButtonSize::Card
                    ),
                    |d| d.justify_start(),
                )
                .gap(match self.size {
                    ButtonSize::Small => px(4.),
                    ButtonSize::Nav => px(12.),
                    _ => Spacing::Xs.px(),
                })
                .relative()
                .children(content)
                .into_any_element();
            let element = element.child(face);
            return if nav {
                controls::navigation(element, phase, response.0, response.1).into_any_element()
            } else if raised && !flat {
                controls::raised(element, role, phase, response.0, response.1).into_any_element()
            } else {
                element.into_any_element()
            };
        }
        // Switch spring state lives below this keyed content wrapper. Keep its
        // path when disabled/reduced motion removes MotionPress, so an in-flight
        // switch is not remounted at its target (or at an old endpoint).
        if self.toggled.is_some() {
            let content_id = format!("press-content-{response_id}");
            return element
                .when(has_light, |d| d.on_hover(hover_light))
                .child(
                    div()
                        .id(gpui::SharedString::from(content_id.clone()))
                        .debug_selector(move || content_id.clone())
                        .flex()
                        .flex_1()
                        .self_stretch()
                        .min_w_0()
                        .max_w_full()
                        .items_center()
                        .justify_center()
                        .gap(Spacing::Xs.px())
                        .relative()
                        .children(content),
                )
                .into_any_element();
        }
        element
            .when(has_light, |d| d.on_hover(hover_light))
            .children(content)
            .into_any_element()
    }
}
