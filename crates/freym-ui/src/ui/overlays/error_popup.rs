use super::Modal;
// Dismissal affects presentation only; the diagnostic error remains available.
use crate::ui::primitives::{Button, ButtonSize, Icons, Text, TextVariant};
use crate::ui::theme::colors::Color;
use gpui::{App, ElementId, FocusHandle, IntoElement, RenderOnce, Window, div, prelude::*, px};

#[derive(Default)]
struct PopupState {
    error: Option<String>,
    dismissed: bool,
    generation: u64,
    expanded: bool,
    restore: Option<FocusHandle>,
}

#[derive(IntoElement)]
pub struct ErrorPopup {
    id: ElementId,
    error: Option<String>,
}
impl ErrorPopup {
    pub fn new(id: impl Into<ElementId>, error: Option<String>) -> Self {
        Self {
            id: id.into(),
            error,
        }
    }
}
impl RenderOnce for ErrorPopup {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = window.use_keyed_state(format!("error-state-{}", self.id), cx, |_, _| {
            PopupState::default()
        });
        let visible = state.update(cx, |state, cx| {
            if state.error != self.error {
                state.error = self.error;
                state.dismissed = false;
                state.expanded = false;
                state.restore = window.focused(cx);
                state.generation = state.generation.wrapping_add(1);
            }
            state
                .error
                .clone()
                .filter(|_| !state.dismissed)
                .map(|error| (error, state.generation))
        });
        let Some((error, generation)) = visible else {
            return div().into_any_element();
        };
        let expanded = state.read(cx).expanded;
        let details = state.clone();
        let dismiss = state.clone();
        let summary = error
            .lines()
            .next()
            .unwrap_or("Operation failed")
            .to_owned();
        div()
            .id(self.id.clone())
            .debug_selector(|| "recorder-error-notification".into())
            .absolute()
            .inset_0()
            .size_full()
            .child(
                Modal::new(
                    format!("{}-{generation}", self.id),
                    "Recording needs attention",
                )
                .close(
                    Button::new("dismiss-error", "Close error dialog")
                        .size(ButtonSize::ListIcon)
                        .icon(Icons::Close)
                        .on_click(move |_, w, cx| {
                            dismiss.update(cx, |s, cx| {
                                if s.generation == generation {
                                    s.dismissed = true;
                                    if let Some(f) = &s.restore {
                                        w.focus(f, cx);
                                    }
                                    cx.notify();
                                }
                            })
                        }),
                )
                .on_dismiss(move |_, w, cx| {
                    state.update(cx, |s, cx| {
                        if s.generation == generation {
                            s.dismissed = true;
                            if let Some(f) = &s.restore {
                                w.focus(f, cx);
                            }
                            cx.notify();
                        }
                    })
                })
                .child(
                    Text::new(summary)
                        .variant(TextVariant::Caption)
                        .color(Color::PrimaryText),
                )
                .child(
                    Button::new(
                        "error-details",
                        if expanded {
                            "Hide diagnostics"
                        } else {
                            "Show diagnostics"
                        },
                    )
                    .size(ButtonSize::Small)
                    .expanded(expanded)
                    .on_click(move |_, _, cx| {
                        details.update(cx, |s, cx| {
                            s.expanded = !s.expanded;
                            cx.notify();
                        })
                    }),
                )
                .when(expanded, |d| {
                    d.child(
                        div()
                            .id("error-technical-details")
                            .debug_selector(|| "error-technical-details".into())
                            .max_h(px(f32::from(window.viewport_size().height) * 0.4))
                            .overflow_y_scroll()
                            .child(Text::new(error).variant(TextVariant::Caption)),
                    )
                }),
            )
            .into_any_element()
    }
}
