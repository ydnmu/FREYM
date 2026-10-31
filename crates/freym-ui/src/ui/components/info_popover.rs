//! Explicit, keyboard-accessible technical help, bounded to the window.
use crate::ui::{
    primitives::*,
    theme::{colors::Color, surface_treatment},
};
use gpui::{
    AnyElement, App, Bounds, ElementId, FocusHandle, IntoElement, Pixels, RenderOnce, SharedString,
    Window, anchored, deferred, div, point, prelude::*, px,
};
struct State {
    open: bool,
    bounds: Option<Bounds<Pixels>>,
    focus: FocusHandle,
    scroll: gpui::ScrollHandle,
}
#[derive(IntoElement)]
pub(crate) struct InfoPopover {
    id: ElementId,
    label: SharedString,
    content: AnyElement,
    help: bool,
}
impl InfoPopover {
    pub(crate) fn new(
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        content: impl IntoElement,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            content: content.into_any_element(),
            help: false,
        }
    }
    pub(crate) fn help(
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        description: impl Into<SharedString>,
    ) -> Self {
        let label = label.into();
        let description = description.into();
        Self {
            id: id.into(),
            label: format!("{label}: {description}").into(),
            content: Text::new(description)
                .variant(TextVariant::Caption)
                .into_any_element(),
            help: true,
        }
    }
    #[allow(dead_code)]
    pub(crate) fn tooltip(mut self) -> Self {
        self.help = true;
        self
    }
}
impl RenderOnce for InfoPopover {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let help = self.help;
        let state = window.use_keyed_state(format!("info-{}", self.id), cx, |window, cx| {
            let focus = cx.focus_handle().tab_stop(true);
            if help {
                cx.on_focus(&focus, window, |s: &mut State, _, cx| {
                    s.open = true;
                    cx.notify();
                })
                .detach();
                cx.on_blur(&focus, window, |s: &mut State, _, cx| {
                    s.open = false;
                    cx.notify();
                })
                .detach();
            }
            State {
                open: false,
                bounds: None,
                focus,
                scroll: gpui::ScrollHandle::new(),
            }
        });
        let open = state.read(cx).open;
        let trigger = state.clone();
        let bounds = state.clone();
        let hover = state.clone();
        let popup_label = self.label.clone();
        let mut root = div()
            .id(format!("info-wrapper-{}", self.id))
            .relative()
            .when(help, |d| {
                d.on_hover(move |hovered, window, cx| {
                    hover.update(cx, |s, cx| {
                        let open = *hovered || (s.open && s.focus.is_focused(window));
                        if s.open != open {
                            s.open = open;
                            cx.notify();
                        }
                    });
                })
            })
            .child(
                Button::new(self.id, self.label)
                    .size(ButtonSize::Small)
                    .when(help, |button| {
                        button
                            .variant(ButtonVariant::Ghost)
                            .content(Icon::new(Icons::Info))
                    })
                    .focus(state.read(cx).focus.clone())
                    .expanded(open)
                    .on_click(move |_, _, cx| {
                        trigger.update(cx, |s, cx| {
                            s.open = help || !s.open;
                            cx.notify();
                        })
                    }),
            )
            .child(
                gpui::canvas(
                    move |b, _, cx| bounds.update(cx, |s, _| s.bounds = Some(b)),
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0()
                .size_full(),
            );
        if open {
            let dismiss = state.clone();
            let keyboard = state.clone();
            root = root
                .key_context("FREYMOverlay")
                .capture_key_down(move |event, window, cx| {
                    let s = keyboard.read(cx);
                    if !s.focus.is_focused(window) {
                        return;
                    }
                    let offset = s.scroll.offset();
                    let bottom = -s.scroll.max_offset().y.abs();
                    let y = match event.keystroke.key.as_str() {
                        "up" => offset.y + px(32.),
                        "down" => offset.y - px(32.),
                        "pageup" => offset.y + px(200.),
                        "pagedown" => offset.y - px(200.),
                        "home" => px(0.),
                        "end" => bottom,
                        _ => return,
                    };
                    s.scroll
                        .set_offset(point(offset.x, y.clamp(bottom, px(0.))));
                    cx.notify(keyboard.entity_id());
                    cx.stop_propagation();
                })
                .on_action(move |_: &DismissOverlay, w, cx| {
                    cx.stop_propagation();
                    dismiss.update(cx, |s, cx| {
                        s.open = false;
                        w.focus(&s.focus, cx);
                        cx.notify();
                    });
                });
            let outside = state.clone();
            let escape = state.clone();
            let position = state
                .read(cx)
                .bounds
                .map(|b| point(b.left(), b.bottom() + px(4.)))
                .unwrap_or_default();
            root = root.child(
                deferred(
                    anchored()
                        .position(position)
                        .snap_to_window_with_margin(px(12.))
                        .child(
                            div()
                                .id("settings-info-popover")
                                .debug_selector(|| "settings-info-popover".into())
                                .aria_label(popup_label)
                                .role(if help {
                                    gpui::Role::Tooltip
                                } else {
                                    gpui::Role::Dialog
                                })
                                .occlude()
                                .w(px(384.))
                                .max_w(
                                    px((f32::from(window.viewport_size().width) - 24.).max(100.)),
                                )
                                .max_h(px(
                                    (f32::from(window.viewport_size().height) * 0.45).min(280.)
                                ))
                                .overflow_y_scroll()
                                .track_scroll(&state.read(cx).scroll)
                                .p(px(12.))
                                .rounded(px(8.))
                                .bg(surface_treatment::overlay_color())
                                .border_1()
                                .border_color(Color::SubtleBorder.get())
                                .key_context("FREYMOverlay")
                                .on_action(move |_: &DismissOverlay, w, cx| {
                                    cx.stop_propagation();
                                    escape.update(cx, |s, cx| {
                                        s.open = false;
                                        w.focus(&s.focus, cx);
                                        cx.notify();
                                    });
                                })
                                .on_mouse_down_out(move |e, _, cx| {
                                    if outside
                                        .read(cx)
                                        .bounds
                                        .is_none_or(|b| !b.contains(&e.position))
                                    {
                                        outside.update(cx, |s, cx| {
                                            s.open = false;
                                            cx.notify();
                                        });
                                    }
                                })
                                .child(self.content),
                        ),
                )
                .with_priority(3),
            );
        }
        root
    }
}
