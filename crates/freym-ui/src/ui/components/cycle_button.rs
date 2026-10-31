//! In-place discrete choices, matching the UI Lab CycleButton interaction.
use crate::ui::primitives::{Button, ButtonSize, RollingText};
use gpui::{App, ElementId, IntoElement, RenderOnce, SharedString, Window, div, prelude::*};
use std::rc::Rc;

#[derive(Clone)]
pub(crate) struct CycleOption {
    pub label: SharedString,
    pub enabled: bool,
}
type Change = Rc<dyn Fn(usize, &mut Window, &mut App)>;
#[derive(IntoElement)]
pub(crate) struct CycleButton {
    id: ElementId,
    label: SharedString,
    selected: usize,
    options: Vec<CycleOption>,
    disabled: bool,
    text_color: Option<gpui::Hsla>,
    rolling: bool,
    change: Change,
}
fn next(options: &[CycleOption], selected: usize, backwards: bool) -> Option<usize> {
    (1..options.len())
        .map(|offset| {
            if backwards {
                (selected + options.len() - offset) % options.len()
            } else {
                (selected + offset) % options.len()
            }
        })
        .find(|&index| options[index].enabled)
}
impl CycleButton {
    pub fn new(
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        selected: usize,
        options: Vec<CycleOption>,
        change: impl Fn(usize, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            selected,
            options,
            disabled: false,
            text_color: None,
            rolling: false,
            change: Rc::new(change),
        }
    }
    pub fn disabled(mut self, value: bool) -> Self {
        self.disabled = value;
        self
    }
    pub fn text_only(mut self, color: gpui::Hsla) -> Self {
        self.text_color = Some(color);
        self
    }
    pub fn rolling_text(mut self) -> Self {
        self.rolling = true;
        self
    }
}
impl RenderOnce for CycleButton {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let focus = window.use_keyed_state(format!("cycle-focus-{}", self.id), cx, |_, cx| {
            cx.focus_handle().tab_stop(true)
        });
        let focus = focus.read(cx).clone();
        let selected = self.selected.min(self.options.len().saturating_sub(1));
        let value = self
            .options
            .get(selected)
            .map(|o| o.label.clone())
            .unwrap_or_default();
        let following = next(&self.options, selected, false);
        let disabled = self.disabled
            || following.is_none()
            || crate::ui::primitives::control_scope::current().inert;
        let click = self.change.clone();
        let key_change = self.change;
        let key_focus = focus.clone();

        let inspect = false;

        let wrapper_id = format!("cycle-{}", self.id);
        let display = if self.rolling {
            RollingText::new(format!("rolling-{}", self.id), value.clone()).into_any_element()
        } else {
            value.clone().into_any_element()
        };
        let control = if let Some(color) = self.text_color {
            let debug = self.id.to_string();
            let design_key = format!("button:{}", self.id);
            let design_label = format!("{}: {}", self.label, value);
            let element = div()
                .id(self.id)
                .debug_selector(move || debug.clone())
                .role(gpui::Role::Button)
                .aria_label(format!("{}: {}", self.label, value))
                .track_focus(&focus)
                .w_full()
                .h(gpui::px(36.))
                .flex()
                .items_center()
                .justify_center()
                .text_size(gpui::px(13.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(color)
                .opacity(if disabled { 0.45 } else { 1. })
                .when(!disabled && !inspect, |d| {
                    d.cursor_pointer()
                        .hover(|d| d.underline())
                        .focus_visible(|d| d.underline())
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            window.focus(&focus, cx);
                            if let Some(index) = following {
                                click(index, window, cx);
                            }
                        })
                })
                .child(display);
            crate::ui::finish_design(
                crate::ui::designable(element, &design_key, &design_label, cx),
                &design_key,
                cx,
            )
        } else {
            Button::new(self.id, format!("{}: {}", self.label, value))
                .focus(focus.clone())
                .size(ButtonSize::Compact)
                .fill_width()
                .disabled(disabled)
                .content(div().min_w_0().truncate().child(display))
                .on_click(move |_, window, cx| {
                    window.focus(&focus, cx);
                    if let Some(index) = following {
                        click(index, window, cx);
                    }
                })
                .into_any_element()
        };
        div()
            .id(wrapper_id)
            .w_full()
            .on_key_down(move |event, window, cx| {
                if disabled || inspect || !key_focus.is_focused(window) || event.is_held {
                    return;
                }
                let backwards = match event.keystroke.key.as_str() {
                    "left" => true,
                    "right" | "enter" | "space" => false,
                    _ => return,
                };
                cx.stop_propagation();
                if let Some(index) = next(&self.options, selected, backwards) {
                    key_change(index, window, cx);
                }
            })
            .child(control)
    }
}
