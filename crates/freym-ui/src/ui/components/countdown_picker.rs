//! Timer cycles presets; the independently focusable number edits in place.
use crate::input::TextInput;
use crate::ui::{primitives::*, theme::colors::Color};
use gpui::{
    App, ElementId, Entity, FocusHandle, Focusable, IntoElement, RenderOnce, Window, div,
    prelude::*, px,
};
use std::rc::Rc;

type Change = Rc<dyn Fn(u8, &mut Window, &mut App)>;
struct State {
    value: u8,
    editing: bool,
    invalid: bool,
    input: Entity<TextInput>,
    focus: FocusHandle,
}
pub(crate) fn next_countdown(value: u8) -> u8 {
    match value {
        0 => 1,
        1 => 3,
        3 => 5,
        5 => 10,
        _ => 0,
    }
}
fn close(state: &Entity<State>, w: &mut Window, cx: &mut App) {
    state.update(cx, |s, cx| {
        s.editing = false;
        s.invalid = false;
        w.focus(&s.focus, cx);
        cx.notify();
    });
}
fn submit(state: &Entity<State>, change: &Change, w: &mut Window, cx: &mut App) {
    let value = state.read(cx).input.read(cx).value().trim().parse::<u8>();
    match value {
        Ok(value) => {
            close(state, w, cx);
            change(value, w, cx);
        }
        Err(_) => state.update(cx, |s, cx| {
            s.invalid = true;
            cx.notify();
        }),
    }
}
#[derive(IntoElement)]
pub(crate) struct CountdownPicker {
    id: ElementId,
    value: u8,
    disabled: bool,
    change: Change,
}
impl CountdownPicker {
    pub fn new(
        id: impl Into<ElementId>,
        value: u8,
        change: impl Fn(u8, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            value,
            disabled: false,
            change: Rc::new(change),
        }
    }
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}
impl RenderOnce for CountdownPicker {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let id = self.id.to_string();
        let value = self.value;
        let state = window.use_keyed_state(format!("countdown-state-{id}"), cx, |window, cx| {
            let input = cx.new(|cx| TextInput::new(value.to_string(), "sec", cx).inline());
            cx.on_blur(&input.focus_handle(cx), window, |s: &mut State, _, cx| {
                if s.editing {
                    s.editing = false;
                    s.invalid = false;
                    cx.notify();
                }
            })
            .detach();
            State {
                value,
                editing: false,
                invalid: false,
                input,
                focus: cx.focus_handle().tab_stop(true),
            }
        });
        if self.disabled || state.read(cx).value != value {
            state.update(cx, |s, _| {
                s.value = value;
                s.editing = false;
                s.invalid = false;
            });
        }
        let editing = state.read(cx).editing;
        let cycle = self.change.clone();
        let cycle_state = state.clone();
        let icon = Button::new(
            self.id,
            if value == 0 {
                "Countdown off".to_owned()
            } else {
                format!("Countdown: {value} seconds")
            },
        )
        .tooltip("Cycle delay: Off / 1 / 3 / 5 / 10 s")
        .size(ButtonSize::ListIcon)
        .variant(ButtonVariant::Ghost)
        .focus(state.read(cx).focus.clone())
        .icon(Icons::Timer)
        .disabled(self.disabled)
        .on_click(move |_, w, cx| {
            close(&cycle_state, w, cx);
            cycle(next_countdown(value), w, cx);
        });
        let outside = state.clone();
        use crate::ui::theme::controls::{Phase, deck_surface};
        let phase = if self.disabled {
            Phase::Disabled
        } else {
            Phase::Rest
        };
        let mut pill = deck_surface(div(), phase)
            .id("countdown-pill")
            .debug_selector(|| "countdown-pill".into())
            .h(px(42.))
            .w_full()
            .px(px(6.))
            .rounded(px(21.))
            .flex()
            .items_center()
            .justify_center()
            .gap(px(2.))
            .when(!self.disabled, |d| {
                d.hover(|d| deck_surface(d, Phase::Hover))
            })
            .on_mouse_down_out(move |_, _, cx| {
                outside.update(cx, |s, cx| {
                    if s.editing {
                        s.editing = false;
                        s.invalid = false;
                        cx.notify();
                    }
                });
            })
            .child(icon);
        if editing {
            let keys = state.clone();
            let change = self.change.clone();
            let dismiss = state.clone();
            pill = pill
                .key_context("FREYMOverlay")
                .on_action(move |_: &DismissOverlay, w, cx| {
                    cx.stop_propagation();
                    close(&dismiss, w, cx);
                })
                .capture_key_down(move |e, w, cx| {
                    if e.keystroke.key == "escape" {
                        cx.stop_propagation();
                        close(&keys, w, cx);
                    } else if e.keystroke.key == "enter" && !e.is_held {
                        cx.stop_propagation();
                        submit(&keys, &change, w, cx);
                    }
                })
                .child(
                    div()
                        .id("countdown-custom")
                        .debug_selector(|| "countdown-custom".into())
                        .w(px(32.))
                        .min_w_0()
                        .border_b_1()
                        .border_color(if state.read(cx).invalid {
                            Color::Error.get()
                        } else {
                            Color::SecondaryText.get()
                        })
                        .aria_label("Countdown seconds, 0–255. Enter confirms; Escape cancels")
                        .child(state.read(cx).input.clone()),
                );
        } else {
            let edit = state.clone();
            pill = pill.child(
                div().w(px(32.)).flex_shrink_0().child(
                    Button::new("countdown-value", format!("Edit {value} second countdown"))
                        .tooltip("Edit countdown seconds")
                        .size(ButtonSize::ListIcon)
                        .variant(ButtonVariant::Ghost)
                        .disabled(self.disabled)
                        .fill_width()
                        .content(
                            div()
                                .text_size(px(13.))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .child(if value == 0 {
                                    "Off".to_owned()
                                } else {
                                    value.to_string()
                                }),
                        )
                        .on_click(move |_, w, cx| {
                            edit.update(cx, |s, cx| {
                                s.input.update(cx, |i, cx| {
                                    i.set_value(value.to_string(), cx);
                                    i.select_value(cx);
                                });
                                s.invalid = false;
                                s.editing = true;
                                w.focus(&s.input.focus_handle(cx), cx);
                                cx.notify();
                            })
                        }),
                ),
            );
        }
        div()
            .id("countdown-slot")
            .debug_selector(|| "countdown-slot".into())
            .w(px(80.))
            .h(px(42.))
            .flex_shrink_0()
            .relative()
            .flex()
            .items_center()
            .child(pill)
            .when(editing && state.read(cx).invalid, |d| {
                d.child(
                    div()
                        .debug_selector(|| "countdown-invalid".into())
                        .absolute()
                        .top(px(42.))
                        .left_0()
                        .text_size(px(10.))
                        .whitespace_nowrap()
                        .text_color(Color::Error.get())
                        .child("0–255 seconds"),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn countdown_cycle_includes_off_and_custom_returns_to_off() {
        let mut v = 0;
        for expected in [1, 3, 5, 10, 0] {
            v = next_countdown(v);
            assert_eq!(v, expected);
        }
        assert_eq!(next_countdown(17), 0);
    }
}

/// The deadline is published by the existing recorder owner, not estimated from a click.
#[derive(IntoElement)]
pub(crate) struct RecordingCountdown {
    deadline: Option<std::time::Instant>,
    compact: bool,
    timed_out: bool,
}
impl RecordingCountdown {
    pub fn new(deadline: Option<std::time::Instant>) -> Self {
        Self {
            deadline,
            compact: false,
            timed_out: false,
        }
    }
    pub fn timed_out(mut self, value: bool) -> Self {
        self.timed_out = value;
        self
    }
    #[allow(dead_code)]
    pub fn compact(mut self) -> Self {
        self.compact = true;
        self
    }
}
pub(crate) fn countdown_remaining(
    deadline: Option<std::time::Instant>,
    now: std::time::Instant,
) -> u64 {
    deadline.map_or(0, |d| {
        d.saturating_duration_since(now)
            .as_nanos()
            .div_ceil(1_000_000_000) as u64
    })
}
struct CountdownValue {
    deadline: Option<std::time::Instant>,
    pending: bool,
    compact: bool,
    generation: u64,
    timed_out: bool,
}
impl gpui::Render for CountdownValue {
    fn render(&mut self, _: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let now = std::time::Instant::now();
        let remaining = countdown_remaining(self.deadline, now);
        if remaining > 0 && !self.pending {
            self.pending = true;
            let deadline = self.deadline;
            let generation = self.generation;
            let delay = deadline
                .unwrap()
                .saturating_duration_since(now)
                .saturating_sub(std::time::Duration::from_secs(remaining - 1))
                + std::time::Duration::from_millis(1);
            cx.spawn(async move |value, cx| {
                cx.background_executor().timer(delay).await;
                let _ = value.update(cx, |value, cx| {
                    if value.generation == generation && value.deadline == deadline {
                        value.pending = false;
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        div()
            .id("recording-countdown")
            .debug_selector(|| "recording-countdown".into())
            .flex()
            .items_center()
            .gap(px(10.))
            .min_w_0()
            .aria_label(format!("Recording countdown {remaining}"))
            .child(
                div()
                    .debug_selector(|| "recording-countdown-value".into())
                    .text_size(px(if self.compact { 16. } else { 28. }))
                    .line_height(px(if self.compact { 22. } else { 34. }))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(RollingText::new("startup-seconds", remaining.to_string())),
            )
            .child(
                Text::new(if self.timed_out {
                    "Startup timed out. Waiting for cleanup."
                } else if self.compact {
                    if remaining > 0 {
                        "Countdown"
                    } else {
                        "Starting"
                    }
                } else if remaining > 0 {
                    "Until capture starts"
                } else {
                    "Waiting for capture readiness"
                })
                .variant(TextVariant::Caption),
            )
    }
}
impl RenderOnce for RecordingCountdown {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = window.use_keyed_state("recording-countdown-state", cx, |_, cx| {
            cx.new(|_| CountdownValue {
                deadline: self.deadline,
                pending: false,
                compact: self.compact,
                generation: 0,
                timed_out: self.timed_out,
            })
        });
        let value = state.read(cx).clone();
        value.update(cx, |value, cx| {
            if value.deadline != self.deadline
                || value.compact != self.compact
                || value.timed_out != self.timed_out
            {
                value.deadline = self.deadline;
                value.timed_out = self.timed_out;
                value.compact = self.compact;
                value.generation += 1;
                value.pending = false;
                cx.notify();
            }
        });
        div().child(value)
    }
}
