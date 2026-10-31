//! Native, Video-owned controls. No media services or settings writes in components.
use crate::{
    ui::{
        primitives::*,
        theme::{
            Theme,
            colors::Color,
            controls::{self, Phase, Role},
            radius::Radius,
            spacing::{Layout, Spacing},
        },
    },
    video::VideoAction,
};
use gpui::{
    AnimationExt, AnyElement, App, ElementId, Entity, FocusHandle, IntoElement, RenderOnce,
    SharedString, Window, anchored, deferred, div, point, prelude::*, px,
};
use std::{cell::Cell, rc::Rc};

struct ValueTooltip(SharedString);
impl gpui::Render for ValueTooltip {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
            .max_w(px(480.))
            .p(px(10.))
            .rounded(px(6.))
            .bg(crate::ui::theme::surface_treatment::overlay_color())
            .child(Text::new(self.0.clone()).variant(TextVariant::Caption))
    }
}

#[derive(Clone)]
pub(crate) struct VideoChoice {
    pub label: String,
    pub action: VideoAction,
    pub selected: bool,
    pub enabled: bool,
    pub description: Option<String>,
    native: bool,
}
impl VideoChoice {
    pub fn new(
        label: impl Into<String>,
        action: VideoAction,
        selected: bool,
        enabled: bool,
    ) -> Self {
        Self {
            label: label.into(),
            action,
            selected,
            enabled,
            description: None,
            native: false,
        }
    }
    pub fn description(mut self, text: impl Into<String>) -> Self {
        self.description = Some(text.into());
        self
    }
    pub fn native(mut self, native: bool) -> Self {
        self.native = native;
        self
    }
}
type SelectHandler = Rc<dyn Fn(&VideoAction, &mut Window, &mut App)>;

struct MenuState {
    open: bool,
    hovered: bool,
    pressed: bool,
    active: usize,
    focus: FocusHandle,
    scroll: gpui::ScrollHandle,
    trigger_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    target: bool,
    from: f32,
    progress: Rc<Cell<f32>>,
    generation: usize,
}
fn close(state: &Entity<MenuState>, cx: &mut App) {
    state.update(cx, |s, cx| {
        s.open = false;
        s.hovered = false;
        s.pressed = false;
        cx.notify();
    });
}
fn step(options: &[VideoChoice], active: usize, backwards: bool) -> usize {
    if options.is_empty() {
        return 0;
    }
    for offset in 1..=options.len() {
        let index = if backwards {
            (active + options.len() - offset) % options.len()
        } else {
            (active + offset) % options.len()
        };
        if options[index].enabled {
            return index;
        }
    }
    active
}

#[derive(IntoElement)]
pub(crate) struct VideoDropdown {
    id: ElementId,
    label: SharedString,
    value: SharedString,
    choices: Vec<VideoChoice>,
    disabled: bool,
    fill_width: bool,
    max_width: Option<f32>,
    min_width: f32,
    native: bool,
    handler: Option<SelectHandler>,
}
impl VideoDropdown {
    pub fn new(
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        value: impl Into<SharedString>,
        choices: Vec<VideoChoice>,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            value: value.into(),
            choices,
            disabled: false,
            fill_width: false,
            max_width: None,
            min_width: 104.,
            native: false,
            handler: None,
        }
    }
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
    pub fn native(mut self, native: bool) -> Self {
        self.native = native;
        self
    }
    pub(crate) fn fill_width(mut self) -> Self {
        self.fill_width = true;
        self
    }
    pub(crate) fn min_width(mut self, width: f32) -> Self {
        self.min_width = width;
        self
    }
    pub(crate) fn max_width(mut self, width: f32) -> Self {
        self.max_width = Some(width);
        self
    }
    pub fn on_select(
        mut self,
        handler: impl Fn(&VideoAction, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.handler = Some(Rc::new(handler));
        self
    }
}
impl RenderOnce for VideoDropdown {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let settings = crate::ui::primitives::control_scope::current().settings;
        let full_width = settings || self.fill_width;
        let style = window.text_style();
        let value_width = window
            .text_system()
            .shape_line(
                self.value.clone(),
                style.font_size.to_pixels(window.rem_size()),
                &[gpui::TextRun {
                    len: self.value.len(),
                    font: style.font(),
                    color: style.color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }],
                None,
            )
            .width;
        let content_width = (f32::from(value_width) + 56.).clamp(
            self.min_width,
            self.max_width.unwrap_or(640.).max(self.min_width),
        );
        let state =
            window.use_keyed_state(format!("video-menu-{}", self.id), cx, |_, cx| MenuState {
                open: false,
                hovered: false,
                pressed: false,
                active: 0,
                focus: cx.focus_handle().tab_stop(true).tab_index(0),
                scroll: gpui::ScrollHandle::new(),
                trigger_bounds: None,
                target: false,
                from: 0.,
                progress: Rc::new(Cell::new(0.)),
                generation: 0,
            });
        let open = !self.disabled && state.read(cx).open;
        let (from, progress, generation) = state.update(cx, |s, _| {
            if s.target != open {
                s.target = open;
                s.from = s.progress.get();
                s.generation += 1;
            }
            (s.from, s.progress.clone(), s.generation)
        });
        let mounted = open || (!cx.reduce_motion() && progress.get() > 0.001);
        let focus = state.read(cx).focus.clone();
        let selected = self.choices.iter().position(|c| c.selected).unwrap_or(0);
        let debug = format!("{}", self.id);
        let key_options = self.choices.clone();
        let key_state = state.clone();
        let key_handler = self.handler.clone();
        let click_state = state.clone();
        let outside_state = state.clone();
        let bounds_state = state.clone();
        let mut trigger = div()
            .relative()
            .id(self.id.clone())
            .debug_selector(move || debug.clone())
            .role(gpui::Role::ComboBox)
            .aria_label(self.label)
            .aria_description(self.value.clone())
            .when(settings, |d| {
                d.tooltip({
                    let value = self.value.clone();
                    move |_, cx| cx.new(|_| ValueTooltip(value.clone())).into()
                })
            })
            .aria_expanded(open)
            .when(full_width && !settings, |d| d.w_full())
            .when(settings, |d| d.w(px(content_width)).max_w_full())
            .when(!full_width, |d| d.w(px(208.)))
            .min_w_0()
            .when(settings, |d| d.min_h(px(34.)).py(px(7.)))
            .when(!settings, |d| {
                d.min_h(px(Layout::COMPACT_CONTROL)).py(px(10.))
            })
            .px(Spacing::Sm.px())
            .flex()
            .items_center()
            .gap(Spacing::Xs.px())
            .rounded(px(10.))
            .border_1()
            .border_color(Color::DefaultBorder.get())
            .bg(Color::PrimarySurface.get())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .whitespace_normal()
                    .line_height(px(17.))
                    .text_color(if self.native {
                        gpui::rgb(0x75cd9b).into()
                    } else {
                        Color::PrimaryText.get()
                    })
                    .child(self.value.clone()),
            )
            .child(
                Icon::new(Icons::Chevron)
                    .size(IconSize::Small)
                    .rotation(if open { 180. } else { 0. }),
            )
            .child(
                gpui::canvas(
                    move |bounds, _, cx| {
                        bounds_state.update(cx, |s, cx| {
                            let changed = s.trigger_bounds != Some(bounds);
                            s.trigger_bounds = Some(bounds);
                            if changed && s.open {
                                cx.notify();
                            }
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            );
        if !self.disabled {
            let hover_state = state.clone();
            let down_state = state.clone();
            let up_state = state.clone();
            let out_state = state.clone();
            trigger = trigger
                .track_focus(&focus)
                .cursor_pointer()
                .focus_visible(|d| d.border_color(Color::ActiveBorder.get()))
                .on_hover(move |hovered, _, cx| {
                    hover_state.update(cx, |s, cx| {
                        s.hovered = *hovered;
                        if !hovered {
                            s.pressed = false;
                        }
                        cx.notify();
                    })
                })
                .on_mouse_down(gpui::MouseButton::Left, move |_, _, cx| {
                    down_state.update(cx, |s, cx| {
                        s.pressed = true;
                        cx.notify();
                    })
                })
                .on_mouse_up(gpui::MouseButton::Left, move |_, _, cx| {
                    up_state.update(cx, |s, cx| {
                        s.pressed = false;
                        cx.notify();
                    })
                })
                .on_mouse_up_out(gpui::MouseButton::Left, move |_, _, cx| {
                    out_state.update(cx, |s, cx| {
                        s.pressed = false;
                        cx.notify();
                    })
                })
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    click_state.update(cx, |s, cx| {
                        s.open = !s.open;
                        s.active = selected;
                        s.scroll.set_offset(point(px(0.), px(0.)));
                        window.focus(&s.focus, cx);
                        cx.notify();
                    });
                });
        } else {
            trigger = trigger
                .opacity(if settings { 0.6 } else { 0.5 })
                .aria_description(format!(
                    "{} · Unavailable for this source or encoder",
                    self.value
                ));
        }
        let phase = if self.disabled {
            Phase::Disabled
        } else if open {
            Phase::Selected
        } else {
            Phase::Rest
        };
        let target = if self.disabled {
            0.
        } else if state.read(cx).pressed {
            2.
        } else if state.read(cx).hovered {
            1.
        } else {
            0.
        };
        let trigger = if settings {
            let phase = if self.disabled {
                Phase::Disabled
            } else if target > 1. {
                Phase::Pressed
            } else if open {
                Phase::Selected
            } else if target > 0. {
                Phase::Hover
            } else {
                Phase::Rest
            };
            controls::settings_control(trigger, Role::Secondary, phase).into_any_element()
        } else {
            trigger
                .with_spring(
                    format!("dropdown-response-{}", self.id),
                    crate::ui::theme::motion::selection(target),
                    move |d, t| {
                        let press = (t - 1.).clamp(0., 1.);
                        controls::raised(
                            d,
                            Role::Secondary,
                            phase,
                            if target > 0. { t.clamp(0., 1.) } else { 0. },
                            press,
                        )
                        .when(full_width, |d| {
                            d.bg(if target > 0. || matches!(phase, Phase::Selected) {
                                Color::HoverSurface.get().opacity(0.7)
                            } else {
                                Color::RowSurface.get().opacity(0.55)
                            })
                            .border_0()
                            .shadow(Vec::new())
                        })
                    },
                )
                .into_any_element()
        };
        let mut root = div()
            .relative()
            .flex_shrink_0()
            .when(full_width && !settings, |d| {
                d.w_full().min_w(px(content_width))
            })
            .when(settings, |d| d.w(px(content_width)).max_w_full())
            .when_some(self.max_width, |d, width| d.max_w(px(width)))
            .when(open, |d| d.key_context("FREYMOverlay"))
            .child(trigger);
        if !self.disabled {
            let dismiss_state = state.clone();
            root = root.on_action(move |_: &DismissOverlay, _, cx| {
                if dismiss_state.read(cx).open {
                    close(&dismiss_state, cx);
                    cx.stop_propagation();
                }
            });
            root = root.capture_key_down(move |event, window, cx| {
                if !key_state.read(cx).focus.contains_focused(window, cx) {
                    return;
                }
                let key = event.keystroke.key.as_str();
                let is_open = key_state.read(cx).open;
                if key == "tab" {
                    close(&key_state, cx);
                    return;
                }
                if matches!(key, "down" | "up" | "home" | "end") {
                    key_state.update(cx, |s, cx| {
                        s.active = if !s.open {
                            selected
                        } else if key == "home" {
                            key_options.iter().position(|o| o.enabled).unwrap_or(0)
                        } else if key == "end" {
                            key_options.iter().rposition(|o| o.enabled).unwrap_or(0)
                        } else {
                            step(&key_options, s.active, key == "up")
                        };
                        s.open = true;
                        s.scroll.scroll_to_item(s.active);
                        cx.notify();
                    });
                    cx.stop_propagation();
                } else if matches!(key, "enter" | "space") && !event.is_held {
                    if is_open {
                        let active = key_state.read(cx).active;
                        if let Some(choice) = key_options.get(active).filter(|o| o.enabled)
                            && let Some(handler) = &key_handler
                        {
                            handler(&choice.action, window, cx);
                        }
                        close(&key_state, cx);
                    } else {
                        key_state.update(cx, |s, cx| {
                            s.open = true;
                            s.active = selected;
                            s.scroll.set_offset(point(px(0.), px(0.)));
                            cx.notify();
                        });
                    }
                    cx.stop_propagation();
                }
            });
        }
        if mounted {
            let trigger = state.read(cx).trigger_bounds.unwrap_or_default();
            let viewport = crate::ui::viewport(window, cx);
            let menu_width = trigger
                .size
                .width
                .max(px(content_width))
                .max(px(1.))
                .min((viewport.width - Spacing::Xs.px() * 2.).max(px(1.)));
            let gap = Spacing::Xxs.px();
            let below = (viewport.height - trigger.bottom() - gap - Spacing::Xs.px()).max(px(0.));
            let above = (trigger.top() - gap - Spacing::Xs.px()).max(px(0.));
            // Prefer a downward menu with a scroll viewport. Only flip when
            // there is insufficient space for even two ordinary options.
            let flip = below < px(160.) && above > below;
            let menu_height = if flip { above } else { below }.min(px(280.)).max(px(1.));
            let anchor = if flip {
                gpui::Anchor::BottomLeft
            } else {
                gpui::Anchor::TopLeft
            };
            let origin = point(
                trigger.center().x - menu_width / 2.,
                if flip {
                    trigger.top() - gap
                } else {
                    trigger.bottom() + gap
                },
            );
            let active = state.read(cx).active;
            let menu_debug = format!("{}-menu", self.id);
            let mut menu = div()
                .id(format!("{}-options", self.id))
                .debug_selector(move || menu_debug.clone())
                .when(open, |d| d.occlude())
                .role(gpui::Role::ListBox)
                .w(menu_width)
                .max_h(menu_height)
                .overflow_y_scroll()
                .track_scroll(&state.read(cx).scroll)
                .p(Spacing::Xxs.px())
                .flex()
                .flex_col()
                .gap(Spacing::Xxs.px())
                .rounded(Radius::Medium.px())
                .border_1()
                .border_color(Color::DefaultBorder.get())
                .bg(crate::ui::theme::surface_treatment::overlay_color())
                .shadow(vec![Theme::shadow()])
                .on_mouse_down_out(move |event, _, cx| {
                    if outside_state
                        .read(cx)
                        .trigger_bounds
                        .is_none_or(|b| !b.contains(&event.position))
                    {
                        close(&outside_state, cx);
                    }
                });
            for (index, choice) in self.choices.into_iter().enumerate() {
                let state = state.clone();
                let handler = self.handler.clone();
                let action = choice.action;
                let highlighted = choice.selected || index == active;
                let foreground = if !choice.enabled {
                    Color::DisabledText
                } else {
                    Color::PrimaryText
                };
                let mut content = Stack::vertical().gap(Spacing::Xxs).child(
                    div()
                        .w_full()
                        .min_w_0()
                        .whitespace_normal()
                        .text_color(if choice.native && choice.enabled {
                            gpui::rgb(0x75cd9b).into()
                        } else {
                            foreground.get()
                        })
                        .child(choice.label.clone()),
                );
                if let Some(description) = choice.description {
                    content = content.child(
                        div()
                            .id(format!("{}-description-{index}", self.id))
                            .w_full()
                            .min_w_0()
                            .aria_description(description.clone())
                            .child(Text::new(description).variant(TextVariant::Muted).color(
                                if foreground == Color::OnAccent {
                                    Color::OnAccent
                                } else {
                                    Color::MutedText
                                },
                            )),
                    );
                }
                let option_debug = format!("{}-option-{index}", self.id);
                menu = menu.child(
                    div()
                        .id(format!("{}-option-{index}", self.id))
                        .debug_selector(move || option_debug.clone())
                        .role(gpui::Role::ListBoxOption)
                        .aria_label(choice.label)
                        .aria_selected(choice.selected)
                        .w_full()
                        .min_w_0()
                        .flex_shrink_0()
                        .p(Spacing::Sm.px())
                        .rounded(Radius::Medium.px())
                        .when(highlighted, |d| {
                            d.bg(if foreground == Color::OnAccent {
                                Color::Accent.get()
                            } else {
                                Color::PrimarySurface.get()
                            })
                        })
                        .child(content)
                        .when(choice.enabled && open, |d| {
                            d.cursor_pointer()
                                .hover(move |d| {
                                    d.bg(if foreground == Color::OnAccent {
                                        Color::AccentHover.get()
                                    } else {
                                        Color::HoverSurface.get()
                                    })
                                })
                                .on_click(move |_, window, cx| {
                                    cx.stop_propagation();
                                    close(&state, cx);
                                    let focus = state.read(cx).focus.clone();
                                    window.focus(&focus, cx);
                                    if let Some(handler) = &handler {
                                        handler(&action, window, cx);
                                    }
                                })
                        })
                        .when(!choice.enabled, |d| {
                            d.aria_description("Unsupported by this recording path")
                        }),
                );
            }
            root = root.child(
                deferred(
                    anchored()
                        .anchor(anchor)
                        .position(origin)
                        .snap_to_window_with_margin(Spacing::Xs.px())
                        .child(menu.with_animation(
                            format!("menu-reveal-{generation}"),
                            crate::ui::theme::motion::expansion(open),
                            move |d, t| {
                                let target = if open { 1. } else { 0. };
                                let value = (from + (target - from) * t).clamp(0., 1.);
                                progress.set(value);
                                d.opacity(value)
                                    .relative()
                                    .top(px((1. - value) * -6.))
                                    .when(!open && value <= 0.001, |d| {
                                        // Drop the closing overlay on the next frame, including
                                        // its hit targets, without leaving an idle animation loop.
                                        d.child(
                                            gpui::canvas(
                                                |_, window, _| window.request_animation_frame(),
                                                |_, _, _, _| {},
                                            )
                                            .size_0(),
                                        )
                                    })
                            },
                        )),
                )
                .with_priority(2),
            );
        }
        root
    }
}

#[derive(IntoElement)]
pub(crate) struct VideoSettingRow {
    label: SharedString,
    description: Option<SharedString>,
    control: AnyElement,
}
impl VideoSettingRow {
    pub(crate) fn into_control(self) -> AnyElement {
        self.control
    }
    pub fn new(label: impl Into<SharedString>, control: impl IntoElement) -> Self {
        Self {
            label: label.into(),
            description: None,
            control: control.into_any_element(),
        }
    }
    pub fn description(mut self, text: impl Into<SharedString>) -> Self {
        self.description = Some(text.into());
        self
    }
}
impl RenderOnce for VideoSettingRow {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let debug = format!("video-row-{}", self.label);
        if crate::ui::primitives::control_scope::current().settings {
            let mut row = super::SettingRow::new(self.label, self.control).wide_control();
            if let Some(description) = self.description {
                row = row.description(description);
            }
            return div()
                .debug_selector(move || debug.clone())
                .w_full()
                .min_w_0()
                .child(row)
                .into_any_element();
        }
        let mut row = Stack::vertical().gap(Spacing::Xxs).child(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .items_center()
                .justify_between()
                .gap(Spacing::Sm.px())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Text::new(self.label).variant(TextVariant::Label)),
                )
                .child(self.control),
        );
        if let Some(description) = self.description {
            row = row.child(Text::new(description).variant(TextVariant::Muted));
        }
        div()
            .debug_selector(move || debug.clone())
            .w_full()
            .py(Spacing::Xs.px())
            .child(row)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keyboard_navigation_skips_unsupported_options_and_wraps() {
        let action = VideoAction::Refresh;
        let choices = vec![
            VideoChoice::new("CBR", action.clone(), true, true),
            VideoChoice::new("CQP", action.clone(), false, false),
            VideoChoice::new("VBR", action, false, true),
        ];
        assert_eq!(step(&choices, 0, false), 2);
        assert_eq!(step(&choices, 2, false), 0);
        assert_eq!(step(&choices, 0, true), 2);
        assert_eq!(step(&[], 0, true), 0);
    }
}
