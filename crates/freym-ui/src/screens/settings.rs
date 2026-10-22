use super::*;
use crate::ui::{
    components::*,
    overlays::*,
    primitives::*,
    theme::{colors::Color, spacing::Spacing},
};

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
        Modal::new(format!("sheet-{sheet:?}"), title)
            .placement(ModalPlacement::Side)
            .on_dismiss(
                cx.listener(|this, _: &(), window, cx| {
                    this.dispatch(Action::CloseSheet, window, cx)
                }),
            )
            .child(content)
            .into_any_element()
    }
    fn sources_sheet(&self, cx: &mut Context<Self>) -> AnyElement {
        let editable = self.model.editable();
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
                                .disabled(!editable)
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
                    editable,
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
                editable,
                cx,
            ))
            .into_any_element()
    }
    fn camera_sheet(&self, cx: &mut Context<Self>) -> AnyElement {
        let editable = self.model.editable();
        let selected = self.model.request.as_ref().and_then(|r| r.camera.as_ref());
        let mut content = Stack::vertical().child(
            Text::new(
                "Add a camera to a screen or area recording. Camera-only recording is unavailable.",
            )
            .variant(TextVariant::Muted),
        );
        if self.model.cameras.is_empty() && !self.model.command_pending {
            content = content.child(Text::new("No camera is available."));
        }
        for camera in &self.model.cameras {
            content = content.child(
                DeviceSelector::new(camera.id.clone(), camera.name.clone())
                    .selected(selected.is_some_and(|s| s.device_id == camera.id))
                    .disabled(!editable)
                    .on_click(self.handler(Action::SelectCamera(camera.id.clone()), cx)),
            );
        }
        if let Some(selected) = selected {
            for format in self.model.formats.iter().filter(|f| f.supported) {
                content = content.child(
                    DeviceSelector::new(
                        format!("camera-format-{}", format.index),
                        format!(
                            "{} × {} · {:.2} FPS",
                            format.width,
                            format.height,
                            format.frame_rate_num as f64 / format.frame_rate_den.max(1) as f64
                        ),
                    )
                    .selected(selected.format_index == format.index)
                    .disabled(!editable)
                    .on_click(self.handler(Action::SelectCameraFormat(format.index), cx)),
                );
            }
            content = content
                .child(self.button(
                    Action::CameraPlacement,
                    "Position & size",
                    None,
                    false,
                    editable,
                    cx,
                ))
                .child(
                    Button::new("remove-camera", "Remove camera overlay")
                        .variant(ButtonVariant::Danger)
                        .disabled(!editable)
                        .on_click(self.handler(Action::RemoveCamera, cx)),
                );
        }
        content.into_any_element()
    }
    fn recovery_sheet(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut content = Stack::vertical().child(Text::new("Recover readable H.264/HEVC video and Opus audio into a new copy. Your original stays intact; an incomplete tail is reported.").variant(TextVariant::Muted));
        for path in &self.model.interrupted {
            content = content.child(
                Button::new(path.clone(), path.clone())
                    .size(ButtonSize::Row)
                    .icon(Icons::Folder)
                    .disabled(self.model.recovery_busy)
                    .on_click(self.handler(Action::RecoverFile(Some(path.clone())), cx)),
            );
        }
        content = content.child(
            Button::new(
                "recover-file",
                if self.model.recovery_busy {
                    "Recovering…"
                } else {
                    "Choose recording…"
                },
            )
            .disabled(self.model.recovery_busy)
            .on_click(self.handler(Action::RecoverFile(None), cx)),
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
        let groups: [(&str, &[_]); 3] = [
            (
                "RECORDING",
                &[
                    (1, "Video", Icons::Screen),
                    (2, "Audio", Icons::Audio),
                    (6, "Camera & Capture", Icons::Camera),
                ],
            ),
            (
                "WORKFLOW",
                &[
                    (3, "Files", Icons::Folder),
                    (7, "Shortcuts", Icons::More),
                    (4, "Performance", Icons::Quality),
                ],
            ),
            (
                "APPLICATION",
                &[
                    (0, "General", Icons::Settings),
                    (5, "Theme", Icons::Quality),
                ],
            ),
        ];
        let mut nav = gpui::div().flex().flex_col().gap(gpui::px(3.));
        for (heading, entries) in groups {
            nav = nav.child(
                gpui::div()
                    .pt(gpui::px(16.))
                    .pb(gpui::px(7.))
                    .px(gpui::px(12.))
                    .text_size(gpui::px(10.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Color::MutedText.get())
                    .child(heading),
            );
            for &(i, label, icon) in entries {
                nav = nav.child(
                    Button::new(format!("settings-category-{i}"), label)
                        .size(ButtonSize::Nav)
                        .variant(ButtonVariant::Ghost)
                        .icon(icon)
                        .selected(self.settings_category == i)
                        .on_click(self.handler(Action::SettingsCategory(i), cx)),
                );
            }
        }
        let category = [
            "General",
            "Video",
            "Audio",
            "Files",
            "Performance",
            "Theme",
            "Camera & Capture",
            "Shortcuts",
        ][self.settings_category.min(7)];
        let description = [
            "Preferences and saved settings.",
            "Resolution, codecs and encoding quality.",
            "Source routing, mixing and recording tracks.",
            "Recording folders and file naming.",
            "Replay buffer and recording resources.",
            "Appearance, button materials and motion.",
            "Camera overlay, capture and mouse pointer.",
            "Keyboard controls for your workflow.",
        ][self.settings_category.min(7)];
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
            6 => Stack::vertical()
                .child(self.camera_sheet(cx))
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
                .into_any_element(),
            7 => self.shortcut_preferences_sheet(cx),
            _ => self.general_preferences_sheet(cx),
        };
        let mut content = Stack::vertical().child(body);
        if let Some(error) = &self.model.error {
            content = content.child(
                Text::new(error.clone())
                    .variant(TextVariant::Caption)
                    .color(Color::Error),
            );
        }
        if self.settings_category == 0 {
            content = content
                .child(Separator)
                .child(Text::new("Settings history").variant(TextVariant::Label))
                .child(self.button(
                    Action::RetrySettings,
                    "Reload saved settings",
                    None,
                    false,
                    self.model.editable() && !self.save_running,
                    cx,
                ))
                .child(self.button(
                    Action::RollbackSettings,
                    "Restore previous settings",
                    None,
                    false,
                    self.model.editable() && !self.save_running && self.save_latest.is_none(),
                    cx,
                ));
        }
        let layout = gpui::div()
            .w_full()
            .h_full()
            .flex()
            .child(
                gpui::div()
                    .id("settings-navigation")
                    .w(gpui::px(218.))
                    .h_full()
                    .flex_shrink_0()
                    .overflow_y_scroll()
                    .bg(Color::PrimarySurface.get())
                    .px(gpui::px(12.))
                    .pt(gpui::px(4.))
                    .pb(gpui::px(16.))
                    .child(nav),
            )
            .child(
                gpui::div()
                    .id("settings-page-body")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_y_scroll()
                    .px(gpui::px(40.))
                    .pb(gpui::px(40.))
                    .child(
                        gpui::div()
                            .h(gpui::px(66.))
                            .flex()
                            .items_center()
                            .border_b_1()
                            .border_color(Color::SubtleBorder.get())
                            .text_size(gpui::px(17.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(category),
                    )
                    .child(
                        gpui::div()
                            .py(gpui::px(24.))
                            .text_size(gpui::px(12.))
                            .text_color(Color::SecondaryText.get())
                            .child(description),
                    )
                    .child(content),
            );
        crate::ui::primitives::control_scope::ControlScope::new(
            crate::ui::primitives::control_scope::ControlStyle {
                settings: true,
                inert: false,
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
        }
        let focus = window.use_keyed_state("settings-page-focus", cx, |_, cx| SettingsFocus {
            back: cx.focus_handle(),
            initialized: false,
            had_overlay: false,
        });
        let overlay = self.number_editor.is_some() || self.shortcut_edit.is_some();
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
        let title = Titlebar::new(crate::ui::components::titlebar::close_button(
            cx.listener(|this, _, w, cx| this.close(w, cx)),
        ))
        .title("Settings")
        .actions(
            Button::new("settings-return", "Back to Recorder")
                .focus(back)
                .size(ButtonSize::Compact)
                .on_click(self.handler(Action::CloseSheet, cx)),
        );
        let mut page = gpui::div()
            .id("settings-workspace")
            .relative()
            .size_full()
            .bg(Color::Background.get())
            .text_color(Color::PrimaryText.get())
            .font_family(crate::ui::theme::typography::FONT_FAMILY)
            .flex()
            .flex_col()
            .child(title)
            .child(
                gpui::div()
                    .flex_1()
                    .min_h_0()
                    .child(self.preferences_sheet(cx)),
            );
        if let Some(editor) = &self.number_editor {
            page = page.child(self.number_sheet(editor, cx));
        } else if let Some(action) = self.shortcut_edit {
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
    fn theme_preferences_sheet(&self, cx: &mut Context<Self>) -> AnyElement {
        Stack::vertical()
            .child(Text::new("Appearance").variant(TextVariant::Heading))
            .child(
                gpui::div()
                    .grid()
                    .grid_cols(2)
                    .gap(Spacing::Sm.px())
                    .children(crate::ui::theme::ThemeId::ALL.map(|theme| {
                        self.button(
                            Action::Theme(theme),
                            theme.label(),
                            None,
                            self.model.preferences.theme == theme,
                            self.preferences_loaded && !self.model.closing,
                            cx,
                        )
                    })),
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
        let mut content =
            Stack::vertical().child(Text::new("Keyboard shortcuts").variant(TextVariant::Heading));
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
        content.into_any_element()
    }
    fn general_preferences_sheet(&self, cx: &mut Context<Self>) -> AnyElement {
        Stack::vertical()
            .child(Text::new("Application").variant(TextVariant::Label))
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
                Text::new(format!(
                    "Installed: {} · Available: {}",
                    self.update_snapshot.installed,
                    self.update_snapshot
                        .available
                        .as_deref()
                        .unwrap_or("Not checked")
                ))
                .variant(TextVariant::Caption),
            )
            .child(Text::new(self.update_snapshot.message()).variant(TextVariant::Caption))
            .child(self.button(
                Action::CheckUpdates,
                if matches!(self.update_snapshot.state, freym_update::State::Failed(_)) {
                    "Retry update"
                } else {
                    "Check for updates"
                },
                Some(Icons::Refresh),
                false,
                self.updates.is_some() && !self.update_running && !self.model.closing,
                cx,
            ))
            .child(Separator)
            .child(Text::new("Workflow").variant(TextVariant::Label))
            .child(self.setting(
                "Start countdown",
                self.button(
                    Action::Countdown,
                    if self.model.preferences.countdown == 0 {
                        "Off".into()
                    } else {
                        format!("{} seconds", self.model.preferences.countdown)
                    },
                    None,
                    false,
                    self.model.editable(),
                    cx,
                ),
            ))
            .child(self.button(
                Action::Compact,
                "Compact mode",
                Some(Icons::Minimize),
                false,
                !self.model.closing,
                cx,
            ))
            .child(self.button(
                Action::MoreOptions,
                "More tools",
                Some(Icons::More),
                false,
                !self.model.closing,
                cx,
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
                Text::new(
                    "Tokens: {date}, {time}, {timestamp}. Leave empty for FREYM_{date}_{time}.",
                )
                .variant(TextVariant::Caption),
            );
        }
        for (label, input) in &editor.fields {
            content = content
                .child(Text::new(*label).variant(TextVariant::Caption))
                .child(input.clone());
        }
        if let Some(slider) = &editor.slider {
            content = content.child(slider.clone());
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
        Modal::new(
            "number-editor",
            if manual {
                "Frame rate"
            } else {
                "Fine adjustments"
            },
        )
        .child(content)
        .into_any_element()
    }
    pub(super) fn diagnostics(&self) -> AnyElement {
        let snapshot = &self.model.snapshot;
        let mut content = Stack::vertical().child(
            Text::new(format!("Recorder: {:?}", snapshot.state)).variant(TextVariant::Caption),
        );
        content = content.child(self.recording_observations());
        for decision in &snapshot.plan_decisions {
            content = content.child(Text::new(decision.clone()).variant(TextVariant::Caption));
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
