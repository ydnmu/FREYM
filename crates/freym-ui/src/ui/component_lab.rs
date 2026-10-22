// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Preview-only catalogue: specimens use the same constructors as the product.
use super::{
    components::{
        AudioKind, AudioSourceRow, CaptureSourceCard, CaptureSourceKind, DeviceSelector,
        QualityMode, QualitySelector, RecordButton, RecordButtonState, RecordingQualityPanel,
        SettingRow, Toggle,
    },
    primitives::{Button, ButtonSize, ButtonVariant, Icons, Text, TextVariant},
    theme::{self, ThemeId, colors::Color},
};
use gpui::{Context, Entity, IntoElement, Render, div, prelude::*, px};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Category {
    Buttons,
    Audio,
    Capture,
    Recording,
    Settings,
    Materials,
}
impl Category {
    const ALL: [Self; 6] = [
        Self::Buttons,
        Self::Audio,
        Self::Capture,
        Self::Recording,
        Self::Settings,
        Self::Materials,
    ];
    fn label(self) -> &'static str {
        match self {
            Self::Buttons => "Buttons & toggles",
            Self::Audio => "Audio sources",
            Self::Capture => "Capture sources",
            Self::Recording => "Recording states",
            Self::Settings => "Settings controls",
            Self::Materials => "Material study",
        }
    }
    fn source(self) -> &'static str {
        match self {
            Self::Buttons => "ui/primitives/button.rs + ui/components/toggle.rs",
            Self::Audio => "ui/components/audio_source_row.rs",
            Self::Capture => "ui/components/capture_source_card.rs",
            Self::Recording => "ui/components/record_button.rs",
            Self::Settings => {
                "ui/components/quality_selector.rs + device_selector.rs + setting_row.rs"
            }
            Self::Materials => "ui/lab_reference.rs (separate material study)",
        }
    }
}

pub(crate) struct ComponentLab {
    category: Category,
    theme: ThemeId,
    disabled: bool,
    selected: bool,
    long_labels: bool,
    connected: bool,
    inline: bool,
    filled: bool,
    checked: bool,
    quality: QualityMode,
    record_state: RecordButtonState,
    activations: usize,
    specimen_width: usize,
    materials: Entity<super::lab_reference::ReferenceSheet>,
}
impl ComponentLab {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        Self {
            category: Category::Buttons,
            theme: ThemeId::AmoledBlack,
            disabled: false,
            selected: false,
            long_labels: false,
            connected: true,
            inline: true,
            filled: true,
            checked: false,
            quality: QualityMode::Automatic,
            record_state: RecordButtonState::Ready,
            activations: 0,
            specimen_width: 560,
            materials: cx.new(|_| super::lab_reference::ReferenceSheet::default()),
        }
    }
    fn specimens(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let row = || div().flex().flex_wrap().items_center().gap(px(12.));
        let column = || div().flex().flex_col().gap(px(16.));
        match self.category {
            Category::Buttons => column()
                .child(row().children([
                    ("lab-real-primary", "Primary", ButtonVariant::Primary),
                    ("lab-real-secondary", "Secondary", ButtonVariant::Secondary),
                    ("lab-real-ghost", "Ghost", ButtonVariant::Ghost),
                    ("lab-real-danger", "Danger", ButtonVariant::Danger),
                ].map(|(id, label, variant)| {
                    Button::new(id, label).variant(variant).disabled(self.disabled).selected(self.selected)
                        .on_click(cx.listener(|this, _, _, cx| { this.activations += 1; cx.notify(); }))
                })))
                .child(row().children([
                    ("lab-size-compact", "Compact", ButtonSize::Compact),
                    ("lab-size-regular", "Regular", ButtonSize::Regular),
                    ("lab-size-dock", "Dock", ButtonSize::Dock),
                ].map(|(id, label, size)| Button::new(id, label).icon(Icons::Settings).size(size).disabled(self.disabled).selected(self.selected))))
                .child(row().child(Toggle::new("lab-real-toggle", "Production toggle", self.checked).disabled(self.disabled)
                    .on_click(cx.listener(|this, _, _, cx| { this.checked = !this.checked; cx.notify(); })))
                    .child(Text::new("Production toggle")))
                .into_any_element(),
            Category::Audio => column().children([
                ("lab-real-mic", "Microphone", AudioKind::Microphone),
                ("lab-real-system", "System output", AudioKind::System),
                ("lab-real-app", "Music application", AudioKind::Application),
            ].map(|(id, label, kind)| {
                let label = if self.long_labels { format!("{label} - USB audio interface with a deliberately long device name") } else { label.to_owned() };
                let item = AudioSourceRow::new(id, label, kind).enabled(self.checked).connected(self.connected)
                    .route(if self.checked { "Both" } else { "Off" }).disabled(self.disabled)
                    .device("Example device").selected(self.selected)
                    .on_click(cx.listener(|this, _, _, cx| { this.checked = !this.checked; cx.notify(); }));
                if self.inline { item.inline() } else { item }
            })).into_any_element(),
            Category::Capture => column().children([
                ("lab-real-display", "Display 1", CaptureSourceKind::Display),
                ("lab-real-window", "Application window", CaptureSourceKind::Window),
                ("lab-real-region", "Selected region", CaptureSourceKind::Region),
                ("lab-real-camera", "USB camera", CaptureSourceKind::Camera),
            ].map(|(id, label, kind)| {
                let label = if self.long_labels { format!("{label} - deliberately long source title for layout checks") } else { label.to_owned() };
                CaptureSourceCard::new(id, label).kind(kind).description("Example source - 2560 x 1440")
                    .disabled(self.disabled).selected(self.selected)
                    .on_click(cx.listener(|this, _, _, cx| { this.selected = !this.selected; cx.notify(); }))
            })).into_any_element(),
            Category::Recording => column()
                .child(row().children([
                    RecordButtonState::Ready, RecordButtonState::Countdown, RecordButtonState::Recording,
                    RecordButtonState::Paused, RecordButtonState::Stopping, RecordButtonState::Failed,
                ].into_iter().enumerate().map(|(i, state)| {
                    Button::new(("lab-record-state", i), format!("{state:?}")).size(ButtonSize::Compact)
                        .selected(self.record_state == state)
                        .on_click(cx.listener(move |this, _, _, cx| { this.record_state = state; cx.notify(); }))
                })))
                .child(RecordButton::new(self.record_state).disabled(self.disabled)
                    .on_click(cx.listener(|this, _, _, cx| { this.activations += 1; cx.notify(); })))
                .child(Text::new("Fixture only. This button never starts a recording.").variant(TextVariant::Muted))
                .into_any_element(),
            Category::Settings => column()
                .child(RecordingQualityPanel::new(self.quality,
                    QualitySelector::new("lab-quality-auto", "Auto").disabled(self.disabled)
                        .on_click(cx.listener(|this, _, _, cx| { this.quality = QualityMode::Automatic; cx.notify(); })),
                    QualitySelector::new("lab-quality-custom", "Custom").disabled(self.disabled)
                        .on_click(cx.listener(|this, _, _, cx| { this.quality = QualityMode::Custom; cx.notify(); })),
                    "Production quality mode control; the full Recorder form remains separate work."))
                .child(SettingRow::new("Camera device", DeviceSelector::new("lab-real-device", "USB camera").disabled(self.disabled)))
                .child(SettingRow::new("Microphone", Toggle::new("lab-setting-toggle", "Microphone", self.checked).disabled(self.disabled)
                    .on_click(cx.listener(|this, _, _, cx| { this.checked = !this.checked; cx.notify(); }))))
                .into_any_element(),
            Category::Materials => self.materials.clone().into_any_element(),
        }
    }
}
impl Render for ComponentLab {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.theme.activate();
        theme::set_filled_buttons(self.filled);
        let mut properties = div().flex().flex_wrap().gap(px(8.));
        for (i, label, value) in [
            (0, "Disabled", self.disabled),
            (1, "Selected", self.selected),
            (2, "Long labels", self.long_labels),
            (3, "Connected", self.connected),
            (4, "Inline audio", self.inline),
            (5, "Filled", self.filled),
        ] {
            properties = properties.child(
                Button::new(("lab-property", i as usize), label)
                    .size(ButtonSize::Compact)
                    .selected(value)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        match i {
                            0 => this.disabled = !this.disabled,
                            1 => this.selected = !this.selected,
                            2 => this.long_labels = !this.long_labels,
                            3 => this.connected = !this.connected,
                            4 => this.inline = !this.inline,
                            _ => this.filled = !this.filled,
                        }
                        cx.notify();
                    })),
            );
        }
        div().id("native-component-lab").size_full().overflow_y_scroll().p(px(24.))
            .font_family(theme::typography::FONT_FAMILY)
            .bg(Color::Background.get()).text_color(Color::PrimaryText.get())
            .child(div().flex().flex_col().gap(px(16.))
                .child(Text::new("GPUI Component Lab").variant(TextVariant::Title))
                .child(Text::new("Production components + editable fixture properties. Hover, press and Tab to inspect interaction states.").variant(TextVariant::Muted))
                .child(div().flex().flex_wrap().gap(px(8.)).children(Category::ALL.into_iter().enumerate().map(|(i, category)| {
                    Button::new(("lab-category", i), category.label()).size(ButtonSize::Compact).selected(self.category == category)
                        .on_click(cx.listener(move |this, _, _, cx| { this.category = category; cx.notify(); }))
                })))
                .child(div().flex().flex_wrap().gap(px(8.)).children(ThemeId::ALL.into_iter().enumerate().map(|(i, theme)| {
                    Button::new(("lab-theme", i), theme.label()).size(ButtonSize::Compact).selected(self.theme == theme)
                        .on_click(cx.listener(move |this, _, _, cx| { this.theme = theme; cx.notify(); }))
                })))
                .child(properties)
                .child(div().flex().flex_wrap().items_center().gap(px(8.))
                    .child(Text::new("Specimen width").variant(TextVariant::Caption))
                    .children([320usize, 560, 960].map(|width| {
                        Button::new(("lab-width", width), format!("{width} px")).size(ButtonSize::Compact)
                            .selected(self.specimen_width == width)
                            .on_click(cx.listener(move |this, _, _, cx| { this.specimen_width = width; cx.notify(); }))
                    })))
                .child(Text::new(format!("Source: crates/freym-ui/src/{}", self.category.source())).variant(TextVariant::Caption))
                .child(div().id("lab-specimens").w(px(self.specimen_width as f32)).max_w(gpui::relative(1.)).min_h(px(260.)).p(px(20.)).rounded(px(12.))
                    .border_1().border_color(Color::DefaultBorder.get()).bg(Color::PrimarySurface.get())
                    .child(super::primitives::control_scope::ControlScope::new(
                        super::primitives::control_scope::ControlStyle {
                            settings: self.category == Category::Settings,
                            inert: false,
                        }, self.specimens(cx))))
                .child(Text::new(format!("Example activations: {} - source edits rebuild and reopen this native preview.", self.activations)).variant(TextVariant::Muted)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext};

    #[gpui::test]
    fn production_components_render_across_categories_and_disabled_button_is_inert(
        cx: &mut TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|_, cx| ComponentLab::new(cx));
        cx.run_until_parked();
        let point = cx.debug_bounds("lab-real-primary").unwrap().center();
        cx.simulate_click(point, Modifiers::default());
        cx.run_until_parked();
        view.update(cx, |lab, cx| {
            assert_eq!(lab.activations, 1);
            lab.disabled = true;
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_click(point, Modifiers::default());
        cx.run_until_parked();
        for category in Category::ALL {
            view.update(cx, |lab, cx| {
                assert_eq!(lab.activations, 1);
                lab.category = category;
                cx.notify();
            });
            cx.run_until_parked();
        }
    }
}
