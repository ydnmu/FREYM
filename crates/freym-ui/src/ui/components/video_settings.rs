//! Native, Video-owned controls. No media services or settings writes in components.
use crate::{
    ui::{
        primitives::*,
        theme::{
            Theme,
            colors::Color,
            radius::Radius,
            spacing::{Layout, Spacing},
        },
    },
    video::VideoAction,
};
use gpui::{
    AnyElement, App, ElementId, Entity, FocusHandle, IntoElement, RenderOnce, SharedString, Window,
    anchored, deferred, div, point, prelude::*, px,
};
use std::rc::Rc;

#[derive(Clone)]
pub(crate) struct VideoChoice {
    pub label: String,
    pub action: VideoAction,
    pub selected: bool,
    pub enabled: bool,
    pub description: Option<String>,
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
        }
    }
    pub fn description(mut self, text: impl Into<String>) -> Self {
        self.description = Some(text.into());
        self
    }
}
type SelectHandler = Rc<dyn Fn(&VideoAction, &mut Window, &mut App)>;

struct MenuState {
    open: bool,
    active: usize,
    focus: FocusHandle,
    scroll: gpui::ScrollHandle,
    trigger_bounds: Option<gpui::Bounds<gpui::Pixels>>,
}
fn close(state: &Entity<MenuState>, cx: &mut App) {
    state.update(cx, |s, cx| {
        s.open = false;
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
            handler: None,
        }
    }
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
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
        let state =
            window.use_keyed_state(format!("video-menu-{}", self.id), cx, |_, cx| MenuState {
                open: false,
                active: 0,
                focus: cx.focus_handle().tab_stop(true).tab_index(0),
                scroll: gpui::ScrollHandle::new(),
                trigger_bounds: None,
            });
        let open = !self.disabled && state.read(cx).open;
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
            .aria_expanded(open)
            .w(px(if settings {
                if f32::from(window.viewport_size().width) < 1000. {
                    200.
                } else {
                    260.
                }
            } else {
                208.
            }))
            .min_w_0()
            .h(px(if settings {
                30.
            } else {
                Layout::COMPACT_CONTROL
            }))
            .px(Spacing::Sm.px())
            .flex()
            .items_center()
            .gap(Spacing::Xs.px())
            .rounded(Radius::Medium.px())
            .border_1()
            .border_color(Color::DefaultBorder.get())
            .bg(Color::PrimarySurface.get())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Text::new(self.value).truncate()),
            )
            .child(
                Icon::new(Icons::Chevron)
                    .size(IconSize::Small)
                    .rotation(if open { 180. } else { 0. }),
            )
            .child(
                gpui::canvas(
                    move |bounds, _, cx| {
                        bounds_state.update(cx, |s, _| s.trigger_bounds = Some(bounds));
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            );
        if !self.disabled {
            trigger = trigger
                .track_focus(&focus)
                .cursor_pointer()
                .focus_visible(|d| d.border_color(Color::ActiveBorder.get()))
                .hover(|d| d.bg(Color::HoverSurface.get()))
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
                .opacity(0.5)
                .aria_description("Unavailable for this source or encoder");
        }
        let mut root = div()
            .relative()
            .flex_shrink_0()
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
        if open {
            let active = state.read(cx).active;
            let menu_debug = format!("{}-menu", self.id);
            let mut menu = div()
                .id(format!("{}-options", self.id))
                .debug_selector(move || menu_debug.clone())
                .occlude()
                .role(gpui::Role::ListBox)
                .w(px(268.))
                .max_h(px(280.))
                .overflow_y_scroll()
                .track_scroll(&state.read(cx).scroll)
                .p(Spacing::Xxs.px())
                .flex()
                .flex_col()
                .gap(Spacing::Xxs.px())
                .rounded(Radius::Medium.px())
                .border_1()
                .border_color(Color::DefaultBorder.get())
                .bg(Color::ElevatedSurface.get())
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
                let foreground = if highlighted && crate::ui::theme::ThemeId::current().is_motion()
                {
                    Color::OnAccent
                } else if !choice.enabled {
                    Color::DisabledText
                } else {
                    Color::PrimaryText
                };
                let mut content = Stack::vertical().gap(Spacing::Xxs).child(
                    Text::new(choice.label.clone())
                        .variant(TextVariant::Label)
                        .color(foreground),
                );
                if let Some(description) = choice.description {
                    content =
                        content.child(Text::new(description).variant(TextVariant::Muted).color(
                            if foreground == Color::OnAccent {
                                Color::OnAccent
                            } else {
                                Color::MutedText
                            },
                        ));
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
                        .when(choice.enabled, |d| {
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
                        .position(
                            state
                                .read(cx)
                                .trigger_bounds
                                .map(|b| {
                                    point(b.right() - px(268.), b.bottom() + Spacing::Xxs.px())
                                })
                                .unwrap_or_default(),
                        )
                        .snap_to_window_with_margin(Spacing::Xs.px())
                        .child(menu),
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
            let mut row = super::SettingRow::new(self.label, self.control);
            if let Some(description) = self.description {
                row = row.description(description);
            }
            return div()
                .debug_selector(move || debug.clone())
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

#[derive(IntoElement)]
pub(crate) struct VideoDisclosure {
    button: Button,
    title: SharedString,
    summary: SharedString,
    content: AnyElement,
    expanded: bool,
}
impl VideoDisclosure {
    pub fn new(
        id: impl Into<ElementId>,
        title: impl Into<SharedString>,
        summary: impl Into<SharedString>,
        content: impl IntoElement,
        expanded: bool,
    ) -> Self {
        let title = title.into();
        Self {
            button: Button::new(id, title.clone())
                .size(ButtonSize::Row)
                .expanded(expanded),
            title,
            summary: summary.into(),
            content: content.into_any_element(),
            expanded,
        }
    }
    pub fn on_click(
        mut self,
        handler: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.button = self.button.on_click(handler);
        self
    }
}
impl RenderOnce for VideoDisclosure {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .rounded(Radius::Medium.px())
            .border_1()
            .border_color(Color::DefaultBorder.get())
            .bg(Color::PrimarySurface.get())
            .child(
                self.button.content(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap(Spacing::Xs.px())
                        .child(
                            Icon::new(Icons::Chevron)
                                .size(IconSize::Small)
                                .rotation(if self.expanded { 180. } else { 0. }),
                        )
                        .child(
                            Stack::vertical()
                                .gap(Spacing::Xxs)
                                .child(Text::new(self.title).variant(TextVariant::Label))
                                .child(
                                    Text::new(self.summary)
                                        .variant(TextVariant::Muted)
                                        .truncate(),
                                ),
                        ),
                ),
            )
            .when(self.expanded, |d| {
                d.child(
                    div()
                        .px(Spacing::Sm.px())
                        .pb(Spacing::Xs.px())
                        .child(self.content),
                )
            })
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
