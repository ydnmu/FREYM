use super::*;
use crate::ui::{
    components::*,
    overlays::*,
    primitives::*,
    theme::{colors::Color, spacing::Spacing},
};
use gpui::AnimationExt;

const SETTINGS_GROUPS: [&[(usize, &str, Icons)]; 3] = [
    &[
        (1, "Video", Icons::Screen),
        (2, "Audio", Icons::Audio),
        (6, "Camera", Icons::Camera),
    ],
    &[
        (3, "Files", Icons::Folder),
        (7, "Shortcuts", Icons::Keyboard),
        (4, "Performance", Icons::Gauge),
    ],
    &[
        (0, "General", Icons::Settings),
        (5, "Theme / Appearance", Icons::Palette),
        (8, "About", Icons::Info),
    ],
];

#[cfg(test)]
#[test]
fn alpha_settings_categories_have_distinct_icons_without_plugins() {
    let entries: Vec<_> = SETTINGS_GROUPS.into_iter().flatten().collect();
    assert_eq!(entries.len(), 9);
    for (i, (index, label, icon)) in entries.iter().enumerate() {
        assert!(*index < 9 && *label != "Plugins");
        assert!(
            entries[..i]
                .iter()
                .all(|(other_index, _, other_icon)| index != other_index && icon != other_icon)
        );
    }
}

impl Home {
    pub(super) fn sheet(&self, sheet: Sheet, cx: &mut Context<Self>) -> AnyElement {
        let title = match sheet {
            Sheet::Sources => "Choose your source",
            Sheet::Settings => "Settings",
            Sheet::Quality => "Video settings",
            Sheet::Options => "More options",
            Sheet::Audio => "Audio settings",
            Sheet::Camera => "Camera overlay",
            Sheet::Recovery => "Recover a recording",
            Sheet::Diagnostics => "Recording details",
        };
        let content = match sheet {
            Sheet::Sources => self.sources_sheet(cx),
            Sheet::Settings => self.preferences_sheet(cx),
            Sheet::Quality => self.quality_options(cx),
            Sheet::Options => self.tools_options(cx),
            Sheet::Audio => self.audio_sheet(cx),
            Sheet::Camera => self.camera_sheet(cx),
            Sheet::Recovery => self.recovery_sheet(cx),
            Sheet::Diagnostics => self.diagnostics(),
        };
        let modal =
            Modal::new(format!("sheet-{sheet:?}"), title)
                .placement(ModalPlacement::Center)
                .width(if matches!(sheet, Sheet::Recovery) {
                    480.
                } else {
                    520.
                })
                .on_dismiss(cx.listener(|this, _: &(), window, cx| {
                    this.dispatch(Action::CloseSheet, window, cx)
                }))
                .child(content)
                .into_any_element();
        if sheet == Sheet::Recovery {
            crate::ui::primitives::control_scope::ControlScope::new(
                crate::ui::primitives::control_scope::ControlStyle {
                    settings: true,
                    ..Default::default()
                },
                modal,
            )
            .into_any_element()
        } else {
            modal
        }
    }
    fn sources_sheet(&self, cx: &mut Context<Self>) -> AnyElement {
        let editable = self.panels_interactive();
        let mut content = Stack::vertical().child(
            SegmentedControl::new()
                .id("source-mode-switch")
                .selected(match self.model.mode {
                    SourceMode::Window => 1,
                    SourceMode::Area => 2,
                    _ => 0,
                })
                .children(
                    [SourceMode::Screen, SourceMode::Window, SourceMode::Area]
                        .into_iter()
                        .map(|mode| {
                            Button::new(format!("{:?}", Action::Source(mode)), mode.label())
                                .size(ButtonSize::Compact)
                                .segment()
                                .icon(match mode {
                                    SourceMode::Screen => Icons::Screen,
                                    SourceMode::Window => Icons::Window,
                                    _ => Icons::Area,
                                })
                                .selected(self.model.mode == mode)
                                .disabled(
                                    !editable
                                        || (!self.model.editable()
                                            && !matches!(
                                                mode,
                                                SourceMode::Screen | SourceMode::Window
                                            )),
                                )
                                .on_click(self.handler(Action::Source(mode), cx))
                        }),
                ),
        );
        if self.model.mode == SourceMode::Window {
            content = content
                .child(
                    Text::new("Select an eligible SDR window. Recording starts separately.")
                        .variant(TextVariant::Muted),
                )
                .child(self.button(
                    Action::PickWindow,
                    "Choose directly on screen",
                    Some(Icons::Window),
                    false,
                    self.model.editable(),
                    cx,
                ));
            for source in &self.model.snapshot.windows {
                let selected = self
                    .model
                    .request
                    .as_ref()
                    .is_some_and(|r| r.target == CaptureTarget::Window(source.id.clone()));
                content = content.child(
                    CaptureSourceCard::new(format!("window-{}", source.id.0), source.title.clone())
                        .kind(CaptureSourceKind::Window)
                        .description(
                            source
                                .process_name
                                .clone()
                                .unwrap_or_else(|| "Window".into()),
                        )
                        .selected(selected)
                        .disabled(!editable)
                        .on_click(self.handler(Action::SelectWindow(source.id.clone()), cx)),
                );
            }
        } else {
            for display in &self.model.snapshot.displays {
                let selected = self
                    .model
                    .request
                    .as_ref()
                    .is_some_and(|r| match &r.target {
                        CaptureTarget::Display(id) => id == &display.id,
                        CaptureTarget::Region(region) => region.display_id == display.id,
                        _ => false,
                    });
                content = content.child(
                    CaptureSourceCard::new(
                        format!("display-{}", display.id.0),
                        display.name.clone(),
                    )
                    .dimensions(display.width, display.height, display.refresh_rate_hz)
                    .selected(selected)
                    .disabled(!editable)
                    .on_click(self.handler(Action::SelectDisplay(display.id.clone()), cx)),
                );
            }
            if self.model.mode == SourceMode::Area {
                content = content.child(
                    Text::new("Choose a display, then drag an area. Esc cancels.")
                        .variant(TextVariant::Muted),
                );
            }
        }
        content
            .child(self.button(
                Action::Refresh,
                "Refresh sources",
                Some(Icons::Refresh),
                false,
                self.model.editable(),
                cx,
            ))
            .into_any_element()
    }
    fn camera_sheet(&self, cx: &mut Context<Self>) -> AnyElement {
        use crate::ui::components::video_settings::{VideoChoice, VideoDropdown};
        use crate::video::VideoAction;
        let editable =
            self.model.editable() && !self.camera_ui.loading && !self.model.settings_blocked;
        let selected = self.selected_camera();
        let available =
            selected.is_some_and(|s| self.model.cameras.iter().any(|c| c.id == s.device_id));
        let dropdown =
            |id, label, value: String, choices, enabled: bool, cx: &mut Context<Self>| {
                VideoDropdown::new(id, label, value, choices)
                    .fill_width()
                    .disabled(!enabled)
                    .on_select(cx.listener(|home, action: &VideoAction, window, cx| {
                        home.dispatch(Action::Video(action.clone()), window, cx)
                    }))
            };
        let devices = self
            .model
            .cameras
            .iter()
            .map(|c| {
                VideoChoice::new(
                    c.name.clone(),
                    VideoAction::CameraDevice(c.id.clone()),
                    selected.is_some_and(|s| s.device_id == c.id),
                    true,
                )
            })
            .collect();
        let device = selected
            .and_then(|s| self.model.cameras.iter().find(|c| c.id == s.device_id))
            .map(|c| c.name.clone())
            .unwrap_or_else(|| {
                if selected.is_some() {
                    "Saved camera disconnected"
                } else {
                    "Choose camera"
                }
                .into()
            });
        let mut content = Stack::vertical().gap(Spacing::Xs).child(SettingRow::new(
            "Device",
            dropdown(
                "camera-device",
                "Camera device",
                device,
                devices,
                editable && !self.model.cameras.is_empty(),
                cx,
            ),
        ));
        if self.model.cameras.is_empty() {
            content = content.child(
                Text::new(if self.camera_ui.loading {
                    "Finding connected cameras…"
                } else {
                    "No camera available. Connect one and refresh."
                })
                .variant(TextVariant::Muted),
            );
        }
        let current = self.current_camera_format();
        for (axis, id, label) in [
            (0, "camera-resolution", "Resolution"),
            (1, "camera-fps", "Frame rate"),
            (2, "camera-pixel-format", "Pixel format"),
        ] {
            let caption = |f: &CameraFormat| match axis {
                0 => format!("{} × {}", f.width, f.height),
                1 => format!(
                    "{:.2} FPS",
                    f.frame_rate_num as f64 / f.frame_rate_den as f64
                ),
                _ => f.subtype.clone(),
            };
            let choices = camera::choices(&self.model.formats, current, axis)
                .into_iter()
                .map(|f| {
                    VideoChoice::new(
                        caption(f),
                        VideoAction::CameraFormat(f.index),
                        current.is_some_and(|c| match axis {
                            0 => c.width == f.width && c.height == f.height,
                            1 => camera::same_rate(c, f),
                            _ => c.subtype == f.subtype,
                        }),
                        true,
                    )
                })
                .collect();
            content = content.child(SettingRow::new(
                label,
                dropdown(
                    id,
                    label,
                    current
                        .map(&caption)
                        .unwrap_or_else(|| "Unavailable".into()),
                    choices,
                    editable && available && self.model.formats.iter().any(camera::usable),
                    cx,
                ),
            ));
        }
        if selected.is_some() && current.is_none() {
            content = content.child(
                Text::new(
                    "Saved capability unavailable. Choose a supported resolution to continue.",
                )
                .variant(TextVariant::Muted),
            );
        }
        content = content.child(
            SettingRow::new(
                "Camera overlay",
                self.toggle(
                    Action::Video(VideoAction::CameraToggle),
                    "Camera overlay",
                    self.model
                        .request
                        .as_ref()
                        .is_some_and(|r| r.camera.is_some()),
                    editable && available && current.is_some(),
                    cx,
                ),
            )
            .description("Use Preview to position the camera."),
        );
        if let Some(selected) = selected {
            content = content
                .child(SettingRow::new(
                    "Position / size",
                    Button::new(
                        "CameraPlacement",
                        format!(
                            "Edit in Preview · {} × {}",
                            selected.placement.width, selected.placement.height
                        ),
                    )
                    .size(ButtonSize::Compact)
                    .disabled(
                        !editable
                            || !available
                            || self
                                .model
                                .request
                                .as_ref()
                                .is_none_or(|r| r.camera.is_none()),
                    )
                    .on_click(self.handler(Action::CameraPlacement, cx)),
                ))
                .child(SettingRow::new(
                    "Device controls",
                    Button::new("CameraDeviceSettings", "Windows camera settings")
                        .size(ButtonSize::Compact)
                        .icon(Icons::Settings)
                        .disabled(!editable || !available || !self.camera_ui.settings_supported)
                        .on_click(self.handler(Action::CameraDeviceSettings, cx)),
                ));
        }
        if self.model.sheet != Some(Sheet::Settings) {
            content = content
                .child(self.camera_refresh(cx))
                .child(self.camera_remove(cx));
        }
        gpui::div()
            .id("camera-settings")
            .debug_selector(|| "camera-settings".into())
            .w_full()
            .child(content)
            .into_any_element()
    }
    fn camera_refresh(&self, cx: &mut Context<Self>) -> Button {
        Button::new(
            "FindCameras",
            if self.camera_ui.loading {
                "Finding cameras…"
            } else {
                "Refresh cameras"
            },
        )
        .size(ButtonSize::Compact)
        .icon(Icons::Refresh)
        .disabled(
            !self.model.editable()
                || self.camera_ui.loading
                || self.model.settings_blocked
                || !cfg!(target_os = "windows"),
        )
        .on_click(self.handler(Action::FindCameras, cx))
    }
    fn camera_remove(&self, cx: &mut Context<Self>) -> AnyElement {
        gpui::div()
            .id("camera-remove-footer")
            .debug_selector(|| "camera-remove-footer".into())
            .w_full()
            .flex_shrink_0()
            .flex()
            .justify_end()
            .p(gpui::px(16.))
            .when(self.selected_camera().is_some(), |d| {
                d.child(
                    Button::new("remove-camera", "Remove camera overlay")
                        .size(ButtonSize::Compact)
                        .variant(ButtonVariant::Danger)
                        .disabled(
                            !self.model.editable()
                                || self.camera_ui.loading
                                || self.model.settings_blocked,
                        )
                        .on_click(self.handler(Action::RemoveCamera, cx)),
                )
            })
            .into_any_element()
    }
    fn recovery_sheet(&self, cx: &mut Context<Self>) -> AnyElement {
        let allowed = !self.model.recovery_busy
            && matches!(
                self.model.snapshot.state,
                RecorderState::Idle | RecorderState::Failed(_)
            );
        let mut list = gpui::div()
            .id("recovery-list")
            .debug_selector(|| "recovery-list".into())
            .w_full()
            .max_h(gpui::px(260.))
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(gpui::px(4.));
        for (index, path) in self.model.interrupted.iter().enumerate() {
            let name = std::path::Path::new(path)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let metadata = self
                .model
                .recovery_metadata
                .get(path)
                .cloned()
                .unwrap_or_else(|| "Timestamp unavailable".into());
            list = list.child(
                Button::new(format!("recovery-entry-{index}"), name.clone())
                    .size(ButtonSize::Row)
                    .selected(self.model.recovery_selected.as_ref() == Some(path))
                    .content(
                        gpui::div()
                            .id(format!("recovery-entry-description-{index}"))
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(gpui::px(3.))
                            .aria_description(path.clone())
                            .child(gpui::div().truncate().text_size(gpui::px(13.)).child(name))
                            .child(
                                gpui::div()
                                    .truncate()
                                    .text_size(gpui::px(11.))
                                    .text_color(Color::SecondaryText.get())
                                    .child(metadata),
                            ),
                    )
                    .disabled(!allowed)
                    .on_click(self.handler(Action::SelectRecovery(path.clone()), cx)),
            );
        }
        if self.model.interrupted.is_empty() {
            list = list.child(
                Text::new("No interrupted recordings found in this folder.")
                    .variant(TextVariant::Caption),
            );
        }
        let mut content = Stack::vertical()
            .child(
                Text::new(
                    "Recover to a new copy. Original files and incomplete tails are preserved.",
                )
                .variant(TextVariant::Muted),
            )
            .child(list)
            .child(
                gpui::div()
                    .flex()
                    .flex_wrap()
                    .gap(gpui::px(8.))
                    .child(
                        Button::new(
                            "recover-selected",
                            if self.model.recovery_busy {
                                "Recovering…"
                            } else {
                                "Recover selected"
                            },
                        )
                        .disabled(!allowed || self.model.recovery_selected.is_none())
                        .on_click(self.handler(
                            Action::RecoverFile(self.model.recovery_selected.clone()),
                            cx,
                        )),
                    )
                    .child(
                        Button::new(
                            "recover-file",
                            if self.model.recovery_busy {
                                "Recovering…"
                            } else {
                                "Choose recording…"
                            },
                        )
                        .disabled(!allowed)
                        .on_click(self.handler(Action::RecoverFile(None), cx)),
                    ),
            );
        if let Some(result) = &self.model.recovery_result {
            content = content.child(
                Text::new(format!(
                    "Recovered copy: {}\n{:.2} seconds · {} packets",
                    result.output_path,
                    result.duration_ms / 1000.,
                    result.recovered_packets
                ))
                .variant(TextVariant::Caption),
            );
            if !result.final_trim_known {
                content = content.child(
                    Text::new("Final audio trim could not be reconstructed.")
                        .variant(TextVariant::Caption)
                        .color(Color::Warning),
                );
            }
            content = content.child(
                Text::new(format!(
                    "Source: {}\nUnparsed tail: {} bytes",
                    result.source_path, result.unparsed_tail_bytes
                ))
                .variant(TextVariant::Muted),
            );
            if let Some(reason) = &result.tail_reason {
                content = content.child(Text::new(reason.clone()).variant(TextVariant::Caption));
            }
            for note in &result.notes {
                content = content.child(Text::new(note.clone()).variant(TextVariant::Caption));
            }
        }
        content.into_any_element()
    }
    pub(super) fn preferences_sheet(&self, cx: &mut Context<Self>) -> AnyElement {
        self.preferences_panels(None, cx)
    }
    fn preferences_panels(
        &self,
        backgrounds: Option<&PanelBackgrounds>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut nav = gpui::div().w_full().flex().flex_col().gap(gpui::px(10.));
        for (group, entries) in SETTINGS_GROUPS.into_iter().enumerate() {
            nav = nav.child(
                gpui::div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap(gpui::px(2.))
                    .when(group > 0, |d| {
                        d.pt(gpui::px(10.))
                            .border_t_1()
                            .border_color(Color::SubtleBorder.get().opacity(0.55))
                    })
                    .children(entries.iter().map(|&(i, label, icon)| {
                        Button::new(format!("settings-category-{i}"), label)
                            .size(ButtonSize::Nav)
                            .variant(ButtonVariant::Ghost)
                            .icon(icon)
                            .selected(self.settings_category == i)
                            .on_click(self.handler(Action::SettingsCategory(i), cx))
                    })),
            );
        }
        let body = match self.settings_category {
            1 => self.video_settings(cx),
            2 => self.audio_sheet(cx),
            3 => Stack::vertical()
                .child(
                    SettingRow::new("Recording folder", self.output_row(cx)).description(
                        self.model
                            .request
                            .as_ref()
                            .map(|r| r.output.directory.clone())
                            .unwrap_or_default(),
                    ),
                )
                .child(self.recording_files_settings(cx))
                .into_any_element(),
            4 => self.recording_performance_settings(cx),
            5 => self.theme_preferences_sheet(cx),
            6 => self.camera_sheet(cx),
            7 => self.shortcut_preferences_sheet(cx),
            8 => self.about_preferences_sheet(),
            _ => self.general_preferences_sheet(cx),
        };
        let heading = match self.settings_category {
            0 => "General",
            3 => "Files",
            4 => "Performance",
            5 => "Theme / Appearance",
            6 => "Camera",
            7 => "Shortcuts",
            8 => "About",
            _ => "",
        };
        let mut content = Stack::vertical().gap(Spacing::Xs);
        if !heading.is_empty() {
            content = content.child(
                gpui::div()
                    .id("settings-page-heading")
                    .debug_selector(|| "settings-page-heading".into())
                    .w_full()
                    .min_h(gpui::px(40.))
                    .flex_shrink_0()
                    .mb(gpui::px(8.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(Text::new(heading).variant(TextVariant::Heading))
                    .when(self.settings_category == 6, |d| {
                        d.child(self.camera_refresh(cx))
                    }),
            );
        }
        let mut content = content.child(body);
        if let Some(error) = &self.model.error {
            content = content.child(
                Text::new(error.clone())
                    .variant(TextVariant::Caption)
                    .color(Color::Error),
            );
        }
        if self.settings_category == 0 {
            content = content.child(SettingRow::new(
                "Settings history",
                Stack::horizontal()
                    .child(self.button(
                        Action::RetrySettings,
                        "Reload",
                        None,
                        false,
                        self.model.editable() && !self.save_running,
                        cx,
                    ))
                    .child(self.button(
                        Action::RollbackSettings,
                        "Restore previous",
                        None,
                        false,
                        self.model.editable() && !self.save_running && self.save_latest.is_none(),
                        cx,
                    )),
            ));
        }
        let branding = gpui::div()
            .id("settings-brand-footer")
            .debug_selector(|| "settings-brand-footer".into())
            .w_full()
            .flex_shrink_0()
            .pt(gpui::px(12.))
            .flex()
            .flex_wrap()
            .items_center()
            .justify_center()
            .gap(gpui::px(12.))
            .child(
                gpui::div()
                    .debug_selector(|| "settings-brand-line".into())
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(gpui::px(8.))
                    .child(
                        gpui::div()
                            .debug_selector(|| "settings-brand-logo".into())
                            .size(gpui::px(36.))
                            .child(crate::assets::brand_mark(36.)),
                    )
                    .child(
                        gpui::div()
                            .debug_selector(|| "settings-brand-wordmark".into())
                            .text_size(gpui::px(24.))
                            .line_height(gpui::px(32.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Color::PrimaryText.get())
                            .child("FREYM"),
                    ),
            )
            .child(
                gpui::div()
                    .debug_selector(|| "settings-version".into())
                    .flex_shrink_0()
                    .text_size(gpui::px(12.))
                    .line_height(gpui::px(18.))
                    .text_color(Color::SecondaryText.get())
                    .child(crate::DISPLAY_VERSION),
            );
        let (save_status, can_retry) = self
            .save_state
            .error
            .as_deref()
            .map(|error| (error, !self.save_running))
            .unwrap_or_else(|| self.settings_save_status());
        let show_save_status = self.save_state.error.is_some()
            || self.model.settings_blocked
            || self.audio_ui.bitrate_draft.is_some()
            || (self.camera_ui.invalid_format
                && self
                    .model
                    .request
                    .as_ref()
                    .is_some_and(|r| r.camera.is_some()));
        let save_bar = gpui::div()
            .id("settings-save-bar")
            .debug_selector(|| "settings-save-bar".into())
            .w_full()
            .px(gpui::px(24.))
            .py(gpui::px(8.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_end()
            .gap(gpui::px(12.))
            .when(save_status != "Saved", |bar| {
                bar.child(
                    gpui::div()
                        .id("settings-save-status")
                        .debug_selector(|| "settings-save-status".into())
                        .min_w_0()
                        .child(
                            Text::new(save_status.to_owned())
                                .variant(TextVariant::Caption)
                                .color(if self.save_state.error.is_some() {
                                    Color::Error
                                } else if self.model.settings_blocked
                                    || self.audio_ui.bitrate_draft.is_some()
                                {
                                    Color::Warning
                                } else {
                                    Color::SecondaryText
                                }),
                        ),
                )
            })
            .when(can_retry, |bar| {
                bar.child(
                    Button::new("settings-retry-save", "Retry saving")
                        .size(ButtonSize::Compact)
                        .on_click(self.handler(Action::RetrySave, cx)),
                )
            });
        let layout = gpui::div()
            .size_full()
            .p(gpui::px(16.))
            .flex()
            .flex_col()
            .gap(gpui::px(12.))
            .child(
                gpui::div()
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .gap(gpui::px(16.))
                    .child(
                        gpui::div()
                            .id("settings-sidebar")
                            .debug_selector(|| "settings-sidebar".into())
                            .w(gpui::relative(0.23))
                            .min_w(gpui::px(176.))
                            .max_w(gpui::px(240.))
                            .h_full()
                            .relative()
                            .flex_shrink_0()
                            .flex()
                            .flex_col()
                            .p(gpui::px(12.))
                            .rounded(gpui::px(14.))
                            .border_1()
                            .map(crate::ui::theme::controls::panel)
                            .when_some(backgrounds, |d, regions| {
                                d.child(regions.track(Some(gpui::px(14.))))
                            })
                            .child(
                                gpui::div()
                                    .id("settings-navigation")
                                    .debug_selector(|| "settings-navigation".into())
                                    .w_full()
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .child(nav),
                            )
                            .child(branding),
                    )
                    .child(
                        gpui::div()
                            .id("settings-content-panel")
                            .debug_selector(|| "settings-content-panel".into())
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .relative()
                            .rounded(gpui::px(14.))
                            .border_1()
                            .map(crate::ui::theme::controls::panel)
                            .when_some(backgrounds, |d, regions| {
                                d.child(regions.track(Some(gpui::px(14.))))
                            })
                            .flex()
                            .flex_col()
                            .when(show_save_status, |panel| panel.child(save_bar))
                            .child(
                                gpui::div()
                                    .id(("settings-page-body", self.settings_category))
                                    .track_scroll(&self.settings_scroll)
                                    .debug_selector(|| "settings-page-body".into())
                                    .w_full()
                                    .flex_1()
                                    .min_h_0()
                                    .min_w_0()
                                    .overflow_y_scroll()
                                    .flex()
                                    .flex_col()
                                    .items_center()
                                    .child(
                                        gpui::div()
                                            .id("settings-content")
                                            .debug_selector(|| "settings-content".into())
                                            .w_full()
                                            .min_w_0()
                                            .max_w(gpui::px(1000.))
                                            .flex_shrink_0()
                                            .px(gpui::px(24.))
                                            .pt(gpui::px(24.))
                                            .pb(gpui::px(24.))
                                            .child(content),
                                    ),
                            )
                            .when(self.settings_category == 6, |panel| {
                                panel.child(self.camera_remove(cx))
                            }),
                    ),
            );
        crate::ui::primitives::control_scope::ControlScope::new(
            crate::ui::primitives::control_scope::ControlStyle {
                home: false,
                settings: true,
                inert: false,
                segment_indicator: false,
            },
            layout,
        )
        .into_any_element()
    }
    pub(super) fn settings_page(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        struct SettingsFocus {
            back: gpui::FocusHandle,
            initialized: bool,
            had_overlay: bool,
            hovered: bool,
        }
        let focus = window.use_keyed_state("settings-page-focus", cx, |_, cx| SettingsFocus {
            back: cx.focus_handle().tab_stop(true),
            initialized: false,
            had_overlay: false,
            hovered: false,
        });
        let overlay = self.number_editor.is_some()
            || (self.shortcut_edit.is_some() && self.settings_category != 7);
        let (back, restore) = focus.update(cx, |s, _| {
            let restore = !overlay && (!s.initialized || s.had_overlay);
            s.initialized = true;
            s.had_overlay = overlay;
            (s.back.clone(), restore)
        });
        if restore {
            let handle = back.clone();
            window.on_next_frame(move |window, cx| window.focus(&handle, cx));
        }
        let hover = focus.read(cx).hovered || back.is_focused(window);
        let hover_state = focus.clone();
        let return_link = gpui::div()
            .id("settings-return")
            .debug_selector(|| "settings-return".into())
            .role(gpui::Role::Button)
            .aria_label("Back to Recorder")
            .track_focus(&back)
            .tab_index(0)
            .h(gpui::px(28.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(gpui::px(8.))
            .text_size(gpui::px(14.))
            .font_weight(gpui::FontWeight::MEDIUM)
            .line_height(gpui::px(20.))
            .cursor_pointer()
            .on_hover(move |over, _, cx| {
                hover_state.update(cx, |s, cx| {
                    if s.hovered != *over {
                        s.hovered = *over;
                        cx.notify();
                    }
                })
            })
            .on_click(self.handler(Action::CloseSheet, cx))
            .child(
                Icon::new(Icons::ArrowLeft)
                    .size(IconSize::Small)
                    .inherit_color(),
            )
            .child(gpui::div().child("Back to Recorder").when(hover, |d| {
                d.shadow(vec![
                    gpui::BoxShadow::new(
                        gpui::px(0.),
                        gpui::px(0.),
                        Color::PrimaryText.get().opacity(0.08),
                    )
                    .blur_radius(gpui::px(10.)),
                ])
            }))
            .with_spring(
                "settings-back-feedback",
                crate::ui::theme::motion::selection(if hover { 1. } else { 0. }),
                |d, amount| d.text_color(Color::PrimaryText.get().opacity(0.68 + 0.32 * amount)),
            )
            .into_any_element();
        let title = Titlebar::new(crate::ui::components::titlebar::close_button(
            cx.listener(|this, _, w, cx| this.close(w, cx)),
        ))
        .transparent_background()
        .leading(return_link)
        .title("");
        let frosted = crate::ui::theme::surface_treatment::is_frosted();
        let backgrounds = PanelBackgrounds::default();
        let mut page = gpui::div()
            .id("settings-workspace")
            .relative()
            .size_full()
            .when(!frosted, |d| d.bg(Color::Background.get()))
            .when(frosted, |d| d.child(backgrounds.shell(None)))
            .text_color(Color::PrimaryText.get())
            .font_family(crate::ui::theme::typography::FONT_FAMILY)
            .flex()
            .flex_col()
            .child(title)
            .child(
                gpui::div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .when(frosted, |d| d.child(backgrounds.track(None)))
                    .child(self.preferences_panels(frosted.then_some(&backgrounds), cx)),
            );
        if let Some(editor) = self.number_editor.as_ref().filter(|e| {
            !(self.model.sheet == Some(Sheet::Settings)
                && self.settings_category == 1
                && matches!(e.purpose, NumberPurpose::Fps | NumberPurpose::Resolution))
        }) {
            page = page.child(self.number_sheet(editor, cx));
        } else if let Some(action) = self.shortcut_edit.filter(|_| self.settings_category != 7) {
            page = page.child(
                Modal::new(
                    "shortcut-editor",
                    format!("Shortcut for {}", configuration::shortcut_label(action)),
                )
                .child(Text::new(
                    "Press your key combination. Backspace disables it. Esc cancels.",
                )),
            );
        }
        page.into_any_element()
    }
    fn about_preferences_sheet(&self) -> AnyElement {
        let notices = Stack::vertical()
            .child(Text::new(include_str!("../../../../NOTICE")).variant(TextVariant::Caption))
            .child(Text::new("GPUI Kit motion · Apache-2.0").variant(TextVariant::Label))
            .child(
                Text::new(include_str!(
                    "../../third_party/gpui-kit-motion/LICENSE-APACHE"
                ))
                .variant(TextVariant::Caption),
            )
            .child(Text::new("Zeron rolling text · MIT").variant(TextVariant::Label))
            .child(
                Text::new(include_str!(
                    "../../third_party/zeron-rolling-text/LICENSE-MIT"
                ))
                .variant(TextVariant::Caption),
            );
        Stack::vertical()
            .child(SettingRow::new("Version", Text::new(crate::DISPLAY_VERSION)))
            .child(SettingRow::new("License", InfoPopover::new(
                "about-license", env!("CARGO_PKG_LICENSE"),
                Text::new(include_str!("../../../../LICENSE")).variant(TextVariant::Caption),
            )))
            .child(SettingRow::new("Source", Text::new("Open source")))
            .child(SettingRow::new("GitHub", Button::new("about-github", "ydnmu/FREYM")
                .size(ButtonSize::Compact)
                .on_click(|_, _, cx| cx.open_url("https://github.com/ydnmu/FREYM"))))
            .child(SettingRow::new("Credits", Text::new("© 2026 FREYM contributors")))
            .child(SettingRow::new("Third-party notices", InfoPopover::new(
                "about-notices", "View notices", notices,
            )))
            .child(Text::new("You may redistribute and modify FREYM under the GPL. Provided without warranty; see the license for terms.").variant(TextVariant::Caption).color(Color::SecondaryText))
            .into_any_element()
    }
    fn theme_preferences_sheet(&self, cx: &mut Context<Self>) -> AnyElement {
        Stack::vertical()
            .child(
                self.setting(
                    "Color",
                    gpui::div()
                        .flex()
                        .flex_wrap()
                        .gap(Spacing::Xs.px())
                        .children(crate::ui::theme::ThemeId::ALL.map(|theme| {
                            self.button(
                                Action::Theme(theme),
                                theme.label(),
                                None,
                                self.model.preferences.theme == theme,
                                self.preferences_loaded && !self.model.closing,
                                cx,
                            )
                        }))
                        .into_any_element(),
                ),
            )
            .child(
                self.setting(
                    "Surface",
                    gpui::div()
                        .flex()
                        .flex_wrap()
                        .gap(Spacing::Xs.px())
                        .children(crate::ui::theme::SurfacePreference::ALL.map(|surface| {
                            self.button(
                                Action::Surface(surface),
                                surface.label(),
                                None,
                                self.model.preferences.surface == surface,
                                self.preferences_loaded && !self.model.closing,
                                cx,
                            )
                        }))
                        .into_any_element(),
                ),
            )
            .child(
                self.setting(
                    "Frosted transparency",
                    crate::ui::components::motion_slider::percentage(
                        self.model.preferences.frosted_transparency,
                        !self.preferences_loaded
                            || self.model.closing
                            || !crate::ui::theme::surface_treatment::is_frosted(),
                        cx.listener(|home, &(value, commit): &(u8, bool), window, cx| {
                            home.dispatch(
                                if commit {
                                    Action::FrostedTransparency(value)
                                } else {
                                    Action::PreviewFrostedTransparency(value)
                                },
                                window,
                                cx,
                            );
                        }),
                    )
                    .into_any_element(),
                ),
            )
            .child(self.setting(
                "Filled buttons",
                self.toggle(
                    Action::FilledButtons,
                    "Filled buttons",
                    self.model.preferences.filled_buttons,
                    self.preferences_loaded && !self.model.closing,
                    cx,
                ),
            ))
            .child(self.setting(
                "Reduced motion",
                self.toggle(
                    Action::Motion,
                    "Reduced motion",
                    self.model.preferences.reduced_motion,
                    true,
                    cx,
                ),
            ))
            .child(self.setting(
                "Low cost materials",
                self.toggle(
                    Action::LowCostEffects,
                    "Low cost materials",
                    self.model.preferences.low_cost_effects,
                    self.preferences_loaded && !self.model.closing,
                    cx,
                ),
            ))
            .into_any_element()
    }
    fn shortcut_preferences_sheet(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut content = Stack::vertical()
            .gap(Spacing::Xs)
            .child(
                Text::new("Esc cancels · Backspace clears · Changes save automatically")
                    .variant(TextVariant::Muted),
            )
            .child(Text::new("Recording").variant(TextVariant::Label));
        for action in [
            ShortcutAction::StartStop,
            ShortcutAction::PauseResume,
            ShortcutAction::SaveReplay,
        ] {
            if !self.shortcut_supported(action) {
                continue;
            }
            let editing = self.shortcut_edit == Some(action);
            let effective = self.recording_binding(action);
            let value = if editing {
                self.shortcut_feedback()
            } else if effective.is_empty() {
                "Unassigned"
            } else {
                &effective
            };
            content = content.child(
                SettingRow::new(
                    configuration::shortcut_label(action),
                    gpui::div()
                        .id(format!("shortcut-setting-{action:?}"))
                        .w_full()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap(Spacing::Xs.px())
                        .child(
                            gpui::div().flex_1().min_w_0().child(
                                Button::new(format!("shortcut-row-{action:?}"), value.to_owned())
                                    .size(ButtonSize::Compact)
                                    .keycap()
                                    .fill_width()
                                    .tooltip(format!(
                                        "Change {} shortcut",
                                        configuration::shortcut_label(action)
                                    ))
                                    .selected(editing)
                                    .disabled(self.model.shortcuts_pending)
                                    .on_click(self.handler(Action::Shortcut(action), cx)),
                            ),
                        )
                        .child(
                            Button::new(format!("shortcut-clear-{action:?}"), "Clear")
                                .size(ButtonSize::Small)
                                .variant(ButtonVariant::Ghost)
                                .disabled(effective.is_empty() || self.model.shortcuts_pending)
                                .on_click(self.handler(Action::ClearShortcut(action), cx)),
                        ),
                )
                .boxed(),
            );
            if let Some(error) = freym_platform::hotkeys::local_conflict(&effective, action) {
                content = content.child(
                    Text::new(error)
                        .variant(TextVariant::Caption)
                        .color(Color::Warning),
                );
            }
        }
        for navigation in [false, true] {
            content = content.child(
                gpui::div().pt(gpui::px(12.)).child(
                    Text::new(if navigation {
                        "Settings navigation · focused window"
                    } else {
                        "Application · focused window"
                    })
                    .variant(TextVariant::Label),
                ),
            );
            for action in shortcuts::ApplicationShortcut::ALL
                .into_iter()
                .filter(|action| action.settings_category().is_some() == navigation)
            {
                let binding = self.application_binding(action);
                let editing = self.shortcut_ui.app_edit == Some(action);
                let value = if editing {
                    self.shortcut_feedback()
                } else if binding.is_empty() {
                    "Unassigned"
                } else {
                    &binding
                };
                content = content.child(
                    SettingRow::new(
                        action.label(),
                        gpui::div()
                            .w_full()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap(Spacing::Xs.px())
                            .child(
                                gpui::div().flex_1().min_w_0().child(
                                    Button::new(
                                        format!("application-shortcut-{action:?}"),
                                        value.to_owned(),
                                    )
                                    .size(ButtonSize::Compact)
                                    .keycap()
                                    .fill_width()
                                    .tooltip(format!("Change {} shortcut", action.label()))
                                    .selected(editing)
                                    .disabled(self.model.shortcuts_pending)
                                    .on_click(
                                        self.handler(Action::ApplicationShortcut(action), cx),
                                    ),
                                ),
                            )
                            .child(
                                Button::new(
                                    format!("application-shortcut-clear-{action:?}"),
                                    "Clear",
                                )
                                .size(ButtonSize::Small)
                                .variant(ButtonVariant::Ghost)
                                .disabled(binding.is_empty() || self.model.shortcuts_pending)
                                .on_click(
                                    self.handler(Action::ClearApplicationShortcut(action), cx),
                                ),
                            ),
                    )
                    .boxed(),
                );
            }
        }
        for error in [&self.shortcut_ui.manager_error, &self.shortcut_ui.error]
            .into_iter()
            .flatten()
        {
            content = content.child(
                Text::new(error.clone())
                    .variant(TextVariant::Caption)
                    .color(Color::Warning),
            );
        }
        content
            .child(
                gpui::div()
                    .w_full()
                    .flex()
                    .justify_end()
                    .pt(gpui::px(12.))
                    .child(
                        Button::new("shortcut-restore-previous", "Restore previous bindings")
                            .size(ButtonSize::Compact)
                            .disabled(
                                self.shortcut_ui.previous.is_none() || self.model.shortcuts_pending,
                            )
                            .on_click(self.handler(Action::RestoreShortcuts, cx)),
                    ),
            )
            .into_any_element()
    }
    fn general_preferences_sheet(&self, cx: &mut Context<Self>) -> AnyElement {
        let update_label = if self.update_running {
            "Checking…"
        } else if matches!(self.update_snapshot.state, freym_update::State::Failed(_)) {
            "Retry update"
        } else {
            "Check for updates"
        };
        Stack::vertical()
            .child(self.setting(
                "Launch at startup",
                if self.startup.failed {
                    self.button(
                        Action::LaunchAtStartup,
                        "Retry",
                        Some(Icons::Refresh),
                        false,
                        !self.startup.pending && !self.model.closing,
                        cx,
                    )
                } else {
                    self.toggle(
                        Action::LaunchAtStartup,
                        "Launch at startup",
                        self.startup.enabled.unwrap_or(false),
                        freym_platform::startup::SUPPORTED
                            && self.preferences_loaded
                            && !self.model.closing
                            && !self.startup.pending
                            && self.startup.enabled.is_some(),
                        cx,
                    )
                },
            ))
            .child(self.setting(
                "Automatic updates",
                self.toggle(
                    Action::AutomaticUpdates,
                    "Automatic updates",
                    self.model.preferences.automatic_updates,
                    self.preferences_loaded && !self.model.closing,
                    cx,
                ),
            ))
            .child(
                SettingRow::new(
                    "Application updates",
                    Button::new("CheckUpdates", update_label)
                        .size(ButtonSize::Compact)
                        .icon(Icons::Refresh)
                        .content(
                            gpui::div()
                                .debug_selector(|| "update-action-label".into())
                                .whitespace_nowrap()
                                .child(update_label),
                        )
                        .disabled(
                            self.updates.is_none() || self.update_running || self.model.closing,
                        )
                        .on_click(self.handler(Action::CheckUpdates, cx)),
                )
                .description(format!(
                    "Installed: {} · Available: {}. {}",
                    self.update_snapshot.installed,
                    self.update_snapshot
                        .available
                        .as_deref()
                        .unwrap_or("Not checked"),
                    self.update_snapshot.message()
                ))
                .single_line_description(),
            )
            .child(self.setting(
                "Show mouse cursor",
                self.toggle(
                    Action::Cursor,
                    "Show mouse cursor",
                    self.model.request.as_ref().is_some_and(|r| r.show_cursor),
                    self.model.editable(),
                    cx,
                ),
            ))
            .child(self.setting(
                "Pointer metadata",
                self.toggle(
                    Action::InteractionMetadata,
                    "Pointer metadata",
                    self.model.preferences.interaction_metadata,
                    self.model.editable() && cfg!(target_os = "windows"),
                    cx,
                ),
            ))
            .into_any_element()
    }

    pub(super) fn quality_options(&self, cx: &mut Context<Self>) -> AnyElement {
        self.video_settings(cx)
    }
    fn tools_options(&self, cx: &mut Context<Self>) -> AnyElement {
        let editable = self.model.editable();
        let mut content = Stack::vertical()
            .child(self.setting(
                "Audio sources & tracks",
                self.button(Action::Audio, "Configure", None, false, editable, cx),
            ))
            .child(self.setting(
                "Replay duration",
                self.button(
                    Action::Replay,
                    format!(
                        "{} seconds",
                        self.model.snapshot.replay_settings.duration_seconds
                    ),
                    None,
                    false,
                    self.model.snapshot.capabilities.can_replay && editable,
                    cx,
                ),
            ))
            .child(self.setting(
                "Replay memory & storage",
                self.button(
                    Action::ReplayMemory,
                    format!(
                        "{} / {} MiB",
                        self.model.snapshot.replay_settings.max_bytes / 1024 / 1024,
                        self.model.snapshot.replay_settings.storage_mib
                    ),
                    None,
                    false,
                    self.model.snapshot.capabilities.can_replay && editable,
                    cx,
                ),
            ))
            .child(self.button(
                Action::ToggleReplay,
                if self.model.snapshot.replay_only {
                    "Stop replay buffer"
                } else {
                    "Start replay buffer"
                },
                None,
                false,
                self.model.snapshot.capabilities.can_replay
                    && !self.model.command_pending
                    && (self.model.snapshot.replay_only || (self.model.can_start())),
                cx,
            ))
            .child(self.button(
                Action::SaveReplay,
                self.model.replay_save_label(),
                None,
                false,
                self.model.can_save_replay(),
                cx,
            ));
        for (action, binding) in [
            (ShortcutAction::StartStop, &self.model.shortcuts.start_stop),
            (
                ShortcutAction::PauseResume,
                &self.model.shortcuts.pause_resume,
            ),
            (
                ShortcutAction::SaveReplay,
                &self.model.shortcuts.save_replay,
            ),
        ] {
            content = content.child(self.setting(
                configuration::shortcut_label(action),
                self.button(
                    Action::Shortcut(action),
                    if binding.is_empty() {
                        "Unassigned"
                    } else {
                        binding
                    },
                    None,
                    false,
                    self.hotkeys.is_some() && !self.model.shortcuts_pending,
                    cx,
                ),
            ));
        }
        content
            .child(Separator)
            .child(self.button(
                Action::Recovery,
                "Recover recording…",
                None,
                false,
                !self.model.recovery_busy,
                cx,
            ))
            .child(self.button(
                Action::Diagnostics,
                "Recording details",
                None,
                false,
                true,
                cx,
            ))
            .child(self.button(
                Action::Refresh,
                "Refresh sources",
                Some(Icons::Refresh),
                false,
                editable,
                cx,
            ))
            .child(
                Text::new(format!(
                    "FREYM {} · GPL-3.0-or-later\nEsc  Cancel     F11  Fullscreen",
                    env!("CARGO_PKG_VERSION")
                ))
                .variant(TextVariant::Muted),
            )
            .into_any_element()
    }
    pub(super) fn number_sheet(&self, editor: &NumberEditor, cx: &mut Context<Self>) -> AnyElement {
        let manual = matches!(editor.purpose, NumberPurpose::Fps);
        let mut content = Stack::vertical();
        if matches!(editor.purpose, NumberPurpose::RecordingFilename) {
            content = content.child(
                Text::new("{date} · {time} · {timestamp}. Leave blank for the default name.")
                    .variant(TextVariant::Caption),
            );
        }
        for (label, input) in &editor.fields {
            content = content
                .child(Text::new(*label).variant(TextVariant::Caption))
                .child(input.clone());
        }
        if let Some(error) = &self.model.error {
            content = content.child(
                Text::new(error.clone())
                    .variant(TextVariant::Caption)
                    .color(Color::Error),
            );
        }
        if manual {
            content = content.child(self.button(
                Action::AutoFps,
                "Match source automatically",
                None,
                false,
                self.model.editable(),
                cx,
            ));
        }
        content = content.child(
            Stack::horizontal()
                .align(Alignment::End)
                .gap(Spacing::Xs)
                .child(self.button(Action::CancelNumber, "Cancel", None, false, true, cx))
                .child(self.button(
                    Action::ApplyNumber,
                    "Apply",
                    None,
                    true,
                    self.model.editable()
                        && (!manual || self.model.snapshot.capabilities.fixed_fps),
                    cx,
                )),
        );
        let modal = Modal::new(
            "number-editor",
            match editor.purpose {
                NumberPurpose::RecordingFilename => "Filename",
                NumberPurpose::Fps => "Frame rate",
                NumberPurpose::Resolution => "Resolution",
                _ => "Recording settings",
            },
        )
        .placement(ModalPlacement::Center)
        .width(
            if matches!(editor.purpose, NumberPurpose::RecordingFilename) {
                440.
            } else {
                480.
            },
        )
        .on_dismiss(
            cx.listener(|this, _, window, cx| this.dispatch(Action::CancelNumber, window, cx)),
        )
        .child(
            gpui::div()
                .w_full()
                .capture_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                    if event.keystroke.key == "enter" && !event.is_held {
                        cx.stop_propagation();
                        this.dispatch(Action::ApplyNumber, window, cx);
                    }
                }))
                .child(content),
        )
        .into_any_element();
        crate::ui::primitives::control_scope::ControlScope::new(
            crate::ui::primitives::control_scope::ControlStyle {
                settings: true,
                ..Default::default()
            },
            modal,
        )
        .into_any_element()
    }
    pub(super) fn diagnostics(&self) -> AnyElement {
        let snapshot = &self.model.snapshot;
        let mut content = Stack::vertical();
        content = content
            .child(self.recording_observations())
            .child(self.recording_file_details());
        if let Some(error) = self.model.error.clone().or_else(|| match &snapshot.state {
            RecorderState::Failed(error) => Some(crate::services::error_details(error)),
            _ => None,
        }) {
            content = content.child(
                Text::new(error)
                    .variant(TextVariant::Caption)
                    .color(Color::Error),
            );
        }
        let mut technical = Stack::vertical().child(
            Text::new(format!(
                "Error log: {}",
                self.services
                    .config
                    .join("last-recording-error.json")
                    .display()
            ))
            .variant(TextVariant::Caption),
        );
        for decision in &snapshot.plan_decisions {
            technical = technical.child(Text::new(decision.clone()).variant(TextVariant::Caption));
        }
        content = content.child(InfoPopover::new(
            "recording-technical-details",
            "Technical details",
            technical,
        ));
        if let Some(notice) = &self.model.notice {
            content = content.child(Text::new(notice.clone()).variant(TextVariant::Caption));
        }
        for warning in &snapshot.warnings {
            content = content.child(
                Text::new(format!("{} {}", warning.message, warning.action))
                    .variant(TextVariant::Caption)
                    .color(Color::Warning),
            );
        }
        if let Some(replay) = &snapshot.last_replay {
            content = content.child(
                Text::new(format!("Replay saved: {}", replay.artifact.path))
                    .variant(TextVariant::Caption),
            );
        }
        content.into_any_element()
    }
}
