use crate::ui::{
    primitives::{DismissOverlay, FocusNext, FocusPrevious, IconButton, Icons, Text, TextVariant},
    theme::{
        Theme,
        colors::Color,
        motion,
        radius::Radius,
        spacing::{Layout, Spacing},
    },
};
use gpui::{
    AnimationExt, AnyElement, App, ElementId, Entity, FocusHandle, IntoElement, ParentElement,
    RenderOnce, SharedString, Window, div, prelude::*, px,
};
use std::{rc::Rc, time::Duration};
type DismissHandler = Rc<dyn Fn(&(), &mut Window, &mut App)>;
#[derive(Clone, Copy, Default)]
pub enum ModalPlacement {
    #[default]
    Center,
    Side,
}
#[derive(IntoElement)]
pub struct Modal {
    id: ElementId,
    title: SharedString,
    close: Option<AnyElement>,
    placement: ModalPlacement,
    content: Vec<AnyElement>,
    dismiss: Option<DismissHandler>,
}
/// Focus and a bounded exit task, scoped to this overlay's lifetime.
struct ModalFocus {
    scope: FocusHandle,
    closing: bool,
    dismiss_task: Option<gpui::Task<()>>,
}
fn dismiss(state: &Entity<ModalFocus>, handler: DismissHandler, window: &mut Window, cx: &mut App) {
    if state.read(cx).closing {
        return;
    }
    if cx.reduce_motion() {
        handler(&(), window, cx);
        return;
    }
    state.update(cx, |this, cx| {
        this.closing = true;
        this.dismiss_task = Some(cx.spawn_in(window, async move |state, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(motion::EXIT_MS))
                .await;
            let _ = state.update_in(cx, |_, window, cx| handler(&(), window, cx));
        }));
        cx.notify();
    });
}
fn move_focus(scope: &FocusHandle, backwards: bool, window: &mut Window, cx: &mut App) {
    if !scope.contains_focused(window, cx) {
        window.focus(scope, cx);
    }
    if backwards {
        window.focus_prev(cx);
    } else {
        window.focus_next(cx);
    }
    let start = window.focused(cx);
    loop {
        if scope.contains_focused(window, cx) {
            break;
        }
        if backwards {
            window.focus_prev(cx);
        } else {
            window.focus_next(cx);
        }
        if window.focused(cx) == start {
            window.focus(scope, cx);
            break;
        }
    }
}
impl Modal {
    pub fn new(id: impl Into<ElementId>, title: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            close: None,
            placement: ModalPlacement::Center,
            content: vec![],
            dismiss: None,
        }
    }
    pub fn placement(mut self, placement: ModalPlacement) -> Self {
        self.placement = placement;
        self
    }
    pub fn close(mut self, close: impl IntoElement) -> Self {
        self.close = Some(close.into_any_element());
        self
    }
    pub fn on_dismiss(mut self, handler: impl Fn(&(), &mut Window, &mut App) + 'static) -> Self {
        self.dismiss = Some(Rc::new(handler));
        self
    }
}
impl ParentElement for Modal {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.content.extend(elements);
    }
}
impl RenderOnce for Modal {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = window.use_keyed_state(format!("focus-{}", self.id), cx, |window, cx| {
            let scope = cx.focus_handle().tab_index(0).tab_stop(false);
            cx.on_next_frame(window, |this: &mut ModalFocus, window, cx| {
                if !this.scope.contains_focused(window, cx) {
                    move_focus(&this.scope, false, window, cx);
                }
            });
            ModalFocus {
                scope,
                closing: false,
                dismiss_task: None,
            }
        });
        let closing = state.read(cx).closing;
        let scope = state.read(cx).scope.clone();
        let backwards_scope = scope.clone();
        let forwards_scope = scope.clone();
        let height = f32::from(window.viewport_size().height);
        let content_height = (height - Spacing::Lg.value() * 4. - Layout::CONTROL_HEIGHT)
            .max(Layout::CONTROL_HEIGHT);
        let width = match self.placement {
            ModalPlacement::Center => Layout::EDITOR_WIDTH,
            ModalPlacement::Side => Layout::SHEET_WIDTH,
        };
        let mut header = div()
            .flex()
            .items_center()
            .justify_between()
            .gap(Spacing::Md.px())
            .child(Text::new(self.title.clone()).variant(TextVariant::Heading));
        if let Some(close) = self.close {
            header = header.child(close);
        } else if let Some(handler) = self.dismiss.clone() {
            let state = state.clone();
            header = header.child(
                IconButton::new("CloseSheet", Icons::Close, "Close panel")
                    .disabled(closing)
                    .on_click(move |_, window, cx| dismiss(&state, handler.clone(), window, cx)),
            );
        }
        let animation_id = format!("modal-{}-{}", self.id, if closing { "out" } else { "in" });
        let scrim_id = format!("scrim-{}-{}", self.id, if closing { "out" } else { "in" });
        let animation = if closing {
            motion::exit()
        } else {
            motion::entrance()
        };
        let side = matches!(self.placement, ModalPlacement::Side);
        div()
            .id(self.id)
            .role(gpui::Role::Dialog)
            .aria_label(self.title)
            .tab_group()
            .track_focus(&scope)
            .when(closing, |d| {
                d.capture_any_mouse_down(|_, _, cx| cx.stop_propagation())
                    .capture_any_mouse_up(|_, _, cx| cx.stop_propagation())
                    .capture_key_down(|_, _, cx| cx.stop_propagation())
            })
            .when_some(self.dismiss, |d, handler| {
                let state = state.clone();
                d.key_context("FREYMOverlay")
                    .on_action(move |_: &DismissOverlay, window, cx| {
                        cx.stop_propagation();
                        dismiss(&state, handler.clone(), window, cx);
                    })
            })
            .on_action(move |_: &FocusNext, window, cx| {
                cx.stop_propagation();
                move_focus(&forwards_scope, false, window, cx);
            })
            .on_action(move |_: &FocusPrevious, window, cx| {
                cx.stop_propagation();
                move_focus(&backwards_scope, true, window, cx);
            })
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            .bg(Color::Scrim.get())
            .flex()
            .items_center()
            .px(Spacing::Lg.px())
            .map(|d| match self.placement {
                ModalPlacement::Center => d.justify_center(),
                ModalPlacement::Side => d.justify_end(),
            })
            .child(
                div()
                    .w(px(width))
                    .max_w_full()
                    .max_h(px(
                        (height - Spacing::Lg.value() * 2.).min(Layout::SHEET_MAX_HEIGHT)
                    ))
                    .p(Spacing::Lg.px())
                    .rounded(Radius::Large.px())
                    .bg(Color::ElevatedSurface.get())
                    .border_1()
                    .border_color(Color::DefaultBorder.get())
                    .when(
                        !(crate::ui::theme::ThemeId::current().is_motion()
                            && crate::ui::theme::low_cost_effects()),
                        |d| d.shadow(vec![Theme::shadow()]),
                    )
                    .map(|d| {
                        if crate::ui::theme::ThemeId::current().is_motion() {
                            crate::ui::theme::material::Material::Frosted
                                .apply(d, crate::ui::theme::low_cost_effects())
                        } else {
                            d
                        }
                    })
                    .flex()
                    .flex_col()
                    .gap(Spacing::Md.px())
                    .child(header)
                    .child(
                        div()
                            .id("sheet-content")
                            .min_h_0()
                            .max_h(px(content_height))
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap(Spacing::Sm.px())
                            .children(self.content),
                    )
                    .with_animation(animation_id, animation.clone(), move |d, t| {
                        let visible = if closing { 1. - t } else { t };
                        let travel = (1. - visible) * motion::PANEL_TRAVEL;
                        d.relative()
                            .left(px(if side { travel } else { 0. }))
                            .top(px(if side { 0. } else { travel * 0.5 }))
                            .opacity(visible)
                    }),
            )
            .with_animation(scrim_id, animation, move |d, t| {
                let visible = if closing { 1. - t } else { t };
                d.bg(Color::Scrim.get().opacity(visible))
            })
    }
}
