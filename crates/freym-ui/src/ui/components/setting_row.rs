use crate::ui::{
    primitives::{Stack, Text, TextVariant},
    theme::spacing::{Layout, Spacing},
};
use gpui::{AnyElement, App, IntoElement, RenderOnce, SharedString, Window, div, prelude::*};
struct DescriptionTooltip(SharedString);
impl gpui::Render for DescriptionTooltip {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
            .id("setting-description-tooltip")
            .debug_selector(|| "setting-description-tooltip".into())
            .max_w(gpui::px(480.))
            .p(gpui::px(10.))
            .rounded(gpui::px(6.))
            .bg(crate::ui::theme::surface_treatment::overlay_color())
            .child(Text::new(self.0.clone()).variant(TextVariant::Muted))
    }
}
#[derive(IntoElement)]
pub struct SettingRow {
    label: SharedString,
    description: Option<SharedString>,
    control: AnyElement,
    wide_control: bool,
    boxed: bool,
    single_line_description: bool,
}
impl SettingRow {
    pub fn new(label: impl Into<SharedString>, control: impl IntoElement) -> Self {
        Self {
            label: label.into(),
            description: None,
            control: control.into_any_element(),
            wide_control: false,
            boxed: false,
            single_line_description: false,
        }
    }
    pub(crate) fn boxed(mut self) -> Self {
        self.boxed = true;
        self
    }
    pub fn description(mut self, description: impl Into<SharedString>) -> Self {
        self.description = Some(description.into());
        self
    }
    pub(crate) fn single_line_description(mut self) -> Self {
        self.single_line_description = true;
        self
    }
    pub(crate) fn wide_control(mut self) -> Self {
        self.wide_control = true;
        self
    }
}
impl RenderOnce for SettingRow {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let settings = crate::ui::primitives::control_scope::current().settings;
        let width = f32::from(crate::ui::viewport(window, cx).width);
        let stacked = (settings
            && (width < Layout::SETTINGS_STACKED_WIDTH || (self.wide_control && width < 1040.)))
            || (!settings && self.wide_control);
        let debug = format!("setting-row-{}", self.label);
        let label_debug = format!("setting-label-{}", self.label);
        let control_debug = format!("setting-control-{}", self.label);
        let description_debug = format!("setting-description-{}", self.label);
        let mut label_line = div()
            .min_w_0()
            .flex()
            .items_center()
            .gap(Spacing::Xs.px())
            .child(
                Text::new(self.label.clone())
                    .variant(TextVariant::Label)
                    .color(if settings {
                        crate::ui::theme::colors::Color::PrimaryText
                    } else {
                        crate::ui::theme::colors::Color::SecondaryText
                    }),
            );
        let mut detail = None;
        if let Some(description) = self.description {
            if settings && !self.single_line_description {
                label_line = label_line.child(super::InfoPopover::help(
                    description_debug,
                    self.label,
                    description,
                ));
            } else if !self.single_line_description && description.len() > 65 {
                detail = Some(
                    div()
                        .child(super::InfoPopover::new(
                            description_debug,
                            "Details",
                            Text::new(description).variant(TextVariant::Caption),
                        ))
                        .into_any_element(),
                );
            } else {
                let text = Text::new(description.clone()).variant(TextVariant::Muted);
                detail = Some(
                    div()
                        .id(description_debug.clone())
                        .debug_selector(move || description_debug.clone())
                        .w_full()
                        .min_w_0()
                        .aria_description(description.clone())
                        .when(self.single_line_description, |d| {
                            d.tooltip(move |_, cx| {
                                cx.new(|_| DescriptionTooltip(description.clone())).into()
                            })
                        })
                        .child(if self.single_line_description {
                            text.truncate()
                        } else {
                            text
                        })
                        .into_any_element(),
                );
            }
        }
        let label = Stack::vertical()
            .gap(Spacing::Xxs)
            .child(label_line)
            .children(detail);
        div()
            .debug_selector(move || debug.clone())
            .w_full()
            .min_w_0()
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_between()
            .gap(Spacing::Md.px())
            .when(settings, |d| {
                d.min_h(gpui::px(46.))
                    .py(gpui::px(6.))
                    .border_b_1()
                    .border_color(
                        crate::ui::theme::colors::Color::SubtleBorder
                            .get()
                            .opacity(0.7),
                    )
            })
            .when(self.boxed, |d| {
                d.min_h(gpui::px(54.))
                    .px(gpui::px(12.))
                    .py(gpui::px(8.))
                    .rounded(gpui::px(8.))
                    .border_0()
                    .bg(gpui::rgb(0x242426))
            })
            .when(stacked, |d| {
                d.flex_col().items_start().gap(Spacing::Sm.px())
            })
            .child(
                div()
                    .debug_selector(move || label_debug.clone())
                    .min_w_0()
                    .when(stacked, |d| d.w_full())
                    .when(!stacked, |d| d.flex_1())
                    .child(label),
            )
            .child(
                div()
                    .debug_selector(move || control_debug.clone())
                    .min_w_0()
                    .when(stacked, |d| d.w_full())
                    .when(settings, |d| {
                        d.flex()
                            .justify_end()
                            .when(stacked, |d| d.w_full().justify_end())
                            .when(!stacked, |d| {
                                d.max_w(gpui::relative(0.72))
                                    .when(self.wide_control, |d| d.w(gpui::px(560.)))
                                    .when(self.boxed, |d| d.w(gpui::px(320.)))
                                    .flex_shrink_0()
                            })
                    })
                    .when(!settings, |d| d.flex_shrink_0())
                    .child(self.control),
            )
    }
}
