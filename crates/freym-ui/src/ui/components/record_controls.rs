use super::RecordButton;
use crate::ui::primitives::{Button, Icon, Icons, control_scope};
use gpui::{
    AnimationExt, AnyElement, App, IntoElement, RenderOnce, SharedString, Window, div, prelude::*,
    px,
};
use std::time::{Duration, Instant};

struct SplitState {
    target: bool,
    from: f32,
    value: f32,
    started: Instant,
}
impl SplitState {
    fn new(target: bool) -> Self {
        let value = if target { 1. } else { 0. };
        Self {
            target,
            from: value,
            value,
            started: Instant::now(),
        }
    }
    fn sample(&self, now: Instant, duration: f32) -> f32 {
        let target = if self.target { 1. } else { 0. };
        let time = now.duration_since(self.started).as_secs_f32() / (duration / 1000.);
        self.from + (target - self.from) * bezier(time.clamp(0., 1.))
    }
}
/// A single reversible timeline keeps the fixed transport's faces and widths together.
#[derive(Clone, Copy)]
pub(super) struct TransportMotion {
    pub target: bool,
    pub progress: f32,
}
// Reference transport easing: CSS cubic-bezier(0.4, 0, 0.2, 1).
fn bezier(time: f32) -> f32 {
    if time <= 0. || time >= 1. {
        return time.clamp(0., 1.);
    }
    let (mut low, mut high) = (0_f32, 1_f32);
    for _ in 0..18 {
        let t = (low + high) / 2.;
        let x = 3. * (1. - t).powi(2) * t * 0.4 + 3. * (1. - t) * t * t * 0.2 + t * t * t;
        if x < time {
            low = t;
        } else {
            high = t;
        }
    }
    let t = (low + high) / 2.;
    t * t * (3. - 2. * t)
}

#[derive(IntoElement)]
pub struct RecordControls {
    record: RecordButton,
    session: Option<(Button, SharedString)>,
    split: bool,
    accessories: Vec<AnyElement>,
    deck_width: Option<f32>,
}
impl RecordControls {
    pub fn new(record: RecordButton) -> Self {
        Self {
            record,
            session: None,
            split: false,
            accessories: vec![],
            deck_width: None,
        }
    }
    pub fn accessories(mut self, accessories: impl IntoIterator<Item = impl IntoElement>) -> Self {
        self.accessories
            .extend(accessories.into_iter().map(IntoElement::into_any_element));
        self
    }
    pub(crate) fn command_deck(mut self, width: f32) -> Self {
        self.deck_width = Some(width);
        self
    }
    pub fn session(mut self, pause: Button, elapsed: impl Into<SharedString>) -> Self {
        self.session = Some((pause, elapsed.into()));
        self
    }
    pub fn split(mut self, split: bool) -> Self {
        self.split = split;
        self
    }
}
impl RenderOnce for RecordControls {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let viewport = crate::ui::viewport(window, cx);
        let geometry = super::RecorderGeometry::new(viewport.width.into(), viewport.height.into());
        let inline = geometry.inline_transport();
        let deck = self.deck_width.is_some();
        let width = self.deck_width.map_or(geometry.transport_width(), |w| {
            // Countdown (80), two 42 px utilities and three 8 px gaps.
            // Keep a usable Pause/Stop face; narrow decks wrap the utilities.
            (w - 188.).max(144.)
        });
        let height = if deck {
            42.
        } else {
            geometry.transport_height()
        };

        let duration = 460.;
        let pause_width = width - height - 2.;
        let split = self.split && self.session.is_some();
        let reduced = cx.reduce_motion();
        let state =
            window.use_keyed_state("record-division-state", cx, |_, _| SplitState::new(split));
        let (progress, active) = state.update(cx, |state, _| {
            let now = Instant::now();
            if state.target != split {
                state.from = state.sample(now, duration);
                state.target = split;
                state.started = now;
            }
            let target = if split { 1. } else { 0. };
            if reduced {
                state.from = target;
            }
            state.value = if reduced {
                target
            } else {
                state.sample(now, duration)
            };
            let active = !reduced
                && state.from != target
                && now.duration_since(state.started) < Duration::from_millis(duration as u64);
            (state.value, active)
        });
        if active {
            window.request_animation_frame();
        }
        let motion = TransportMotion {
            target: split,
            progress,
        };
        let paused = self.record.is_paused();
        let record = if deck {
            self.record.compact()
        } else {
            self.record
        };
        let mut transport = div()
            .id("record-primary-transport")
            .debug_selector(|| "record-primary-transport".into())
            .w(px(width))
            .h(px(height))
            .flex_shrink_0()
            .rounded(px(10.))
            .bg(crate::ui::theme::colors::Color::RowSurface
                .get()
                .opacity(0.30))
            .flex()
            .gap(px(2. * progress))
            .items_center();
        if let Some((pause, elapsed)) = self.session {
            let pause = if deck {
                pause
                    .size(crate::ui::primitives::ButtonSize::RecordCompact)
                    .fill_width()
            } else {
                pause
            };
            let face = div()
                .id("record-pause-face")
                .debug_selector(|| "record-pause-face".into())
                .flex()
                .items_center()
                .justify_center()
                .whitespace_nowrap()
                .gap(px(8.))
                .opacity(((progress - 0.45) / 0.45).clamp(0., 1.))
                .relative()
                .left(px(4. * (1. - progress)))
                .child(Icon::new(if paused { Icons::Play } else { Icons::Pause }))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(5.))
                        .when(paused && width >= 164., |d| {
                            d.child(div().text_size(px(11.)).child("Resume"))
                        })
                        .child(div().text_size(px(11.)).child(elapsed))
                        .with_animation(
                            format!("record-pause-label-{paused}"),
                            crate::ui::theme::motion::state_change(),
                            |d, t| d.opacity(t),
                        ),
                );
            let mut style = control_scope::current();
            style.inert |= !split || progress < 0.45;
            let pause_disabled = pause.is_disabled() || progress < 0.99;
            transport = transport.child(
                div()
                    .id("record-pause-slot")
                    .debug_selector(|| "record-pause-slot".into())
                    .h(px(height))
                    .w(px(pause_width * progress))
                    .opacity((progress / 0.8).clamp(0., 1.))
                    .min_w_0()
                    .flex_shrink_0()
                    .child(super::record_button::hover_frame(
                        "pause-border-beam",
                        div()
                            .size_full()
                            .overflow_hidden()
                            .child(control_scope::ControlScope::new(
                                style,
                                div().w(px(pause_width)).child(pause.content(face)),
                            ))
                            .into_any_element(),
                        pause_disabled,
                        true,
                        window,
                        cx,
                    )),
            );
        }
        transport = transport.child(
            div()
                .id("record-primary-slot")
                .debug_selector(|| "record-primary-slot".into())
                .h(px(height))
                .w(px(width - (pause_width + 2.) * progress))
                .min_w_0()
                .flex_shrink_0()
                .child(record.transport(motion)),
        );
        if self.deck_width.is_some() {
            let wrap = self.deck_width.is_some_and(|width| width < 144. + 188.);
            let mut accessories = self.accessories.into_iter();
            let countdown = accessories.next();
            let deck = div()
                .id("recorder-transport")
                .debug_selector(|| "recorder-transport".into())
                .w_full()
                .min_w_0()
                .min_h(px(42.))
                .when(!wrap, |d| d.h(px(42.)).overflow_x_scroll())
                .when(wrap, |d| d.flex_wrap())
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .id("deck-primary-row")
                        .debug_selector(|| "deck-primary-row".into())
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .flex_shrink_0()
                        .children(countdown)
                        .child(transport),
                )
                .child(
                    div()
                        .id("deck-accessories")
                        .ml_auto()
                        .flex_shrink_0()
                        .debug_selector(|| "deck-accessories".into())
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .children(accessories),
                );
            return crate::ui::finish_design(
                crate::ui::designable(deck, "command-deck", "Command Deck", cx).w_full(),
                "command-deck",
                cx,
            );
        }
        let element = div()
            .id("recorder-transport")
            .debug_selector(|| "recorder-transport".into())
            .w_full()
            .flex()
            .items_center()
            .justify_center()
            .when(!inline, |d| d.flex_col())
            .gap(px(8.))
            .child(transport)
            .when(!self.accessories.is_empty(), |d| {
                d.child(
                    div()
                        .id("recorder-accessories")
                        .debug_selector(|| "recorder-accessories".into())
                        .flex_shrink_0()
                        .gap(px(8.))
                        .when(geometry.wrap_accessories(), |d| {
                            d.grid()
                                .grid_cols(2)
                                .w(px(geometry.dock_width(false) * 2. + 8.))
                        })
                        .when(!geometry.wrap_accessories(), |d| {
                            d.flex().items_center().w(px(geometry.accessory_width()))
                        })
                        .children(self.accessories),
                )
            });
        crate::ui::finish_design(
            crate::ui::designable(element, "recorder-transport", "Recording controls", cx),
            "recorder-transport",
            cx,
        )
    }
}
