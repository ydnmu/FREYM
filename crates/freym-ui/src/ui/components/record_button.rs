use crate::ui::{
    primitives::{Button, ButtonSize, ButtonVariant, clickable},
    theme::motion,
};
use gpui::{AnimationExt, App, IntoElement, RenderOnce, SharedString, Window, div, prelude::*, px};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordButtonState {
    Ready,
    Countdown,
    Recording,
    Paused,
    Stopping,
    Failed,
}
impl RecordButtonState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ready => "Record",
            Self::Countdown => "Cancel preparation",
            Self::Recording => "Stop recording",
            Self::Paused => "Finish recording",
            Self::Stopping => "Finalizing recording…",
            Self::Failed => "Reset recorder",
        }
    }
    pub fn active(self) -> bool {
        matches!(self, Self::Countdown | Self::Recording | Self::Paused)
    }
}
#[derive(IntoElement)]
pub struct RecordButton {
    button: Button,
    state: RecordButtonState,
    label: SharedString,
    icon_only: bool,
}
impl RecordButton {
    pub fn new(state: RecordButtonState) -> Self {
        Self {
            button: Button::new("record-button", state.label())
                .size(ButtonSize::Record)
                .variant(ButtonVariant::Primary),
            state,
            label: state.label().into(),
            icon_only: false,
        }
    }
    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = label.into();
        self.button = self.button.label(self.label.clone());
        self
    }
    pub(crate) fn icon_only(mut self) -> Self {
        self.icon_only = true;
        self.button = self.button.size(ButtonSize::Dock);
        self
    }
}
clickable!(RecordButton);
impl RenderOnce for RecordButton {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let indicator = super::record_indicator::RecordIndicator {
            state: self.state,
            phase: 0.,
        };
        let marker = indicator.into_any_element();
        let disabled = self.button.is_disabled() || self.state == RecordButtonState::Stopping;
        let button = if self.state == RecordButtonState::Recording {
            self.button.variant(ButtonVariant::Record)
        } else if matches!(
            self.state,
            RecordButtonState::Countdown
                | RecordButtonState::Paused
                | RecordButtonState::Stopping
                | RecordButtonState::Failed
        ) {
            self.button.variant(ButtonVariant::Secondary)
        } else {
            self.button
        };
        button
            .content(marker)
            .map(|button| {
                if self.icon_only {
                    button
                } else {
                    button.content(div().child(self.label).with_animation(
                        format!("record-state-{:?}", self.state),
                        motion::state_change(),
                        |d, t| {
                            d.relative()
                                .top(px((1. - t) * motion::STATE_TRAVEL))
                                .opacity(t)
                        },
                    ))
                }
            })
            .disabled(disabled)
    }
}
