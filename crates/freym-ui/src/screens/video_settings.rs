use super::*;
use crate::{
    ui::{
        components::{SegmentedControl, video_settings::*},
        primitives::*,
        theme::spacing::Spacing,
    },
    video::{VideoAction, VideoEdit, VideoMode, VideoSection, adapter::VideoCapabilities},
};
use frame_settings::policy::RecordingPlan;

fn automatic_caption(detail: Option<String>) -> String {
    detail.map_or_else(
        || "Automatic (pending)".into(),
        |value| format!("Automatic ({value})"),
    )
}
pub(super) fn automatic_codec(plan: Option<&RecordingPlan>) -> String {
    automatic_caption(plan.map(|plan| match plan.encoder.codec.as_str() {
        "h264" => "H.264".into(),
        "hevc" => "HEVC".into(),
        "av1" => "AV1".into(),
        value => value.to_uppercase(),
    }))
}
pub(super) fn automatic_encoder(
    caps: Option<&VideoCapabilities>,
    plan: Option<&RecordingPlan>,
) -> String {
    automatic_caption(plan.and_then(|plan| {
        caps?
            .encoders
            .iter()
            .find(|e| e.candidate.id == plan.encoder.id)
            .map(|e| e.label.clone())
    }))
}
pub(super) fn automatic_quality(plan: Option<&RecordingPlan>) -> String {
    automatic_caption(plan.map(|plan| format!("{:?}", plan.quality)))
}

impl Home {
    fn video_dropdown(
        &self,
        id: &'static str,
        label: &'static str,
        value: String,
        choices: Vec<VideoChoice>,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> VideoDropdown {
        VideoDropdown::new(id, label, value, choices)
            .min_width(match id {
                "video-resolution" => 176.,
                "video-fps" => 128.,
                _ => 264.,
            })
            .max_width(640.)
            .disabled(!enabled)
            .on_select(cx.listener(|this, action: &VideoAction, window, cx| {
                this.dispatch(Action::Video(action.clone()), window, cx)
            }))
    }
    pub(super) fn video_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let mode_switch = SegmentedControl::new()
            .id("video-mode-switch")
            .compact()
            .selected(if self.video.mode == VideoMode::Easy {
                0
            } else {
                1
            })
            .children(
                [(VideoMode::Easy, "Easy"), (VideoMode::Advanced, "Advanced")].map(
                    |(mode, label)| {
                        Button::new(format!("video-mode-{mode:?}"), label)
                            .size(ButtonSize::Compact)
                            .segment()
                            .selected(self.video.mode == mode)
                            .disabled(self.model.closing)
                            .on_click(self.handler(Action::Video(VideoAction::Mode(mode)), cx))
                    },
                ),
            );
        let mut content = Stack::vertical().gap(Spacing::Xs).child(
            gpui::div()
                .id("video-mode-header")
                .flex_shrink_0()
                .min_h(gpui::px(40.))
                .mb(gpui::px(8.))
                .debug_selector(|| "video-mode-header".into())
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .gap(gpui::px(12.))
                .child(
                    gpui::div()
                        .debug_selector(|| "video-mode-heading".into())
                        .flex_1()
                        .min_w_0()
                        .child(Text::new("Video").variant(TextVariant::Heading)),
                )
                .child(
                    gpui::div()
                        .id("video-mode-control")
                        .debug_selector(|| "video-mode-control".into())
                        .w(gpui::px(220.))
                        .max_w(gpui::relative(0.62))
                        .flex_shrink_0()
                        .child(mode_switch),
                ),
        );
        let Some(request) = &self.model.request else {
            return content
                .child(
                    Text::new("Choose a capture source to configure video.")
                        .variant(TextVariant::Muted),
                )
                .into_any_element();
        };
        let context = self.video_context();
        let caps = context
            .as_ref()
            .and_then(|c| self.video.capabilities_for(c));
        let editable = self.model.editable() && !self.model.settings_blocked;
        let plan = caps.map(|c| c.plan(request));
        let resolved = plan.as_ref().and_then(|p| p.as_ref().ok());
        if self.video.mode == VideoMode::Easy {
            content = content
                .child(self.video_capture_pair(request, editable, cx))
                .child(self.video_quality(request, caps, resolved, editable, cx))
                .child(self.video_codec(request, caps, resolved, editable, "Video format", cx))
                .child(self.video_encoder(request, caps, resolved, editable, cx))
                .child(self.setting(
                    "Show Mouse Cursor",
                    self.toggle(
                        Action::Cursor,
                        "Show Mouse Cursor",
                        request.show_cursor,
                        editable,
                        cx,
                    ),
                ));
        } else {
            for (section, title) in [
                (VideoSection::Geometry, "Size and timing"),
                (VideoSection::Encoder, "Encoding"),
                (VideoSection::Preset, "Profile and keyframes"),
                (VideoSection::Rate, "Rate and bitrate"),
                (VideoSection::Analysis, "Quality tools"),
            ] {
                content = content
                    .child(
                        gpui::div()
                            .w_full()
                            .flex_shrink_0()
                            .pt(gpui::px(12.))
                            .child(Text::new(title).variant(TextVariant::Label)),
                    )
                    .child(self.video_section(section, request, caps, resolved, editable, cx));
            }
            let mut details = Stack::vertical()
                .gap(Spacing::None)
                .child(Text::new("Backend values · read only").variant(TextVariant::Label));
            if cfg!(target_os = "windows") {
                details = details
                    .child(
                        self.video_fixed(
                            "Peak bitrate",
                            resolved
                                .map(|p| {
                                    if p.peak_bitrate_bps == 0 {
                                        "Not used (CQP)".into()
                                    } else {
                                        format!("{} bit/s", p.peak_bitrate_bps)
                                    }
                                })
                                .unwrap_or_else(|| "Not qualified".into()),
                        ),
                    )
                    .child(self.video_fixed(
                        "B-frames",
                        if resolved.is_some() {
                            "0"
                        } else {
                            "Not qualified"
                        },
                    ))
                    .child(self.video_fixed(
                        "Lookahead",
                        if resolved.is_some() {
                            "0 frames"
                        } else {
                            "Not qualified"
                        },
                    ));
            }
            for section in [VideoSection::Color, VideoSection::Capture] {
                details =
                    details.child(self.video_section(section, request, caps, resolved, false, cx));
            }
            content = content.child(gpui::div().pt(gpui::px(12.)).child(
                crate::ui::components::InfoPopover::new(
                    "video-source-details",
                    "Backend details · read only",
                    details,
                ),
            ));
        }
        let status = if self.video.probing.is_some() {
            "Checking hardware…".into()
        } else if let Some(error) = &self.video.probe_error {
            error.clone()
        } else if let Some(caps) = caps {
            caps.adapter.clone()
        } else {
            "Check available encoders.".into()
        };
        if self.video.mode == VideoMode::Advanced {
            content = content.child(
                gpui::div().pt(gpui::px(16.)).child(
                    crate::ui::components::SettingRow::new(
                        "Hardware compatibility",
                        Button::new("video-check-hardware", "Check hardware")
                            .size(ButtonSize::Compact)
                            .icon(Icons::Refresh)
                            .disabled(!editable || self.video.probing.is_some())
                            .on_click(self.handler(Action::Video(VideoAction::Refresh), cx)),
                    )
                    .description(status),
                ),
            );
        }
        if let Some(Err(error)) = plan {
            content = content.child(
                Text::new(format!("Selection needs attention: {error}"))
                    .variant(TextVariant::Muted),
            );
        }
        content.into_any_element()
    }
    fn video_resolution(
        &self,
        request: &RecordingRequest,
        editable: bool,
        cx: &mut Context<Self>,
    ) -> VideoSettingRow {
        if let Some(editor) = self
            .number_editor
            .as_ref()
            .filter(|e| matches!(e.purpose, NumberPurpose::Resolution))
        {
            return VideoSettingRow::new("Resolution", self.video_inline_number(editor, cx));
        }
        let native = self.video_native_size();
        let current = match request.resolution {
            Resolution::Native => native,
            Resolution::Exact(d) => Some(d),
        };
        let choices = native
            .into_iter()
            .map(|d| {
                VideoChoice::new(
                    format!("{} × {}", d.width, d.height),
                    VideoAction::Edit(VideoEdit::NativeResolution),
                    current == Some(d),
                    true,
                )
                .native(true)
            })
            .collect();
        VideoSettingRow::new(
            "Resolution",
            self.video_dropdown(
                "video-resolution",
                "Resolution",
                current
                    .map(|d| format!("{} × {}", d.width, d.height))
                    .unwrap_or_else(|| "Detecting source size…".into()),
                choices,
                editable,
                cx,
            )
            .fill_width()
            .native(current.is_some() && current == native),
        )
    }
    fn video_capture_pair(
        &self,
        request: &RecordingRequest,
        editable: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        gpui::div()
            .debug_selector(|| "video-capture-pair".into())
            .w_full()
            .child(
                crate::ui::components::SettingRow::new(
                    "Resolution × Frame rate",
                    gpui::div()
                        .debug_selector(|| "video-capture-controls".into())
                        .w_full()
                        .max_w(gpui::relative(1.))
                        .min_w_0()
                        .flex()
                        .items_center()
                        .flex_wrap()
                        .justify_end()
                        .gap(gpui::px(8.))
                        .child(
                            gpui::div()
                                .flex_shrink_0()
                                .max_w_full()
                                .child(self.video_resolution(request, editable, cx).into_control()),
                        )
                        .child(
                            gpui::div()
                                .flex_shrink_0()
                                .max_w_full()
                                .child(self.video_fps(request, editable, cx).into_control()),
                        ),
                )
                .description("Source resolution; the recording frame rate may be reduced.")
                .wide_control(),
            )
            .into_any_element()
    }
    fn video_fps(
        &self,
        request: &RecordingRequest,
        editable: bool,
        cx: &mut Context<Self>,
    ) -> VideoSettingRow {
        if let Some(editor) = self
            .number_editor
            .as_ref()
            .filter(|e| matches!(e.purpose, NumberPurpose::Fps))
        {
            return VideoSettingRow::new("Frame rate", self.video_inline_number(editor, cx));
        }
        let native = self
            .model
            .source_rate_hz()
            .filter(|r| r.is_finite() && *r > 0.);
        let native_label = native
            .map(|n| format!("{n:.2} FPS"))
            .unwrap_or_else(|| "Source rate unavailable".into());
        let mut choices = vec![
            VideoChoice::new(
                native_label.clone(),
                VideoAction::Edit(VideoEdit::Fps(None)),
                request.fps_mode == FpsMode::AutoMatchSource,
                native.is_some(),
            )
            .native(native.is_some()),
        ];
        for fps in [24, 30, 60, 120, 144, 180] {
            choices.push(VideoChoice::new(
                format!("{fps} FPS"),
                VideoAction::Edit(VideoEdit::Fps(Some(fps * 1000))),
                request.fps_mode == FpsMode::Fixed(f64::from(fps)),
                self.validate_video_rate(f64::from(fps)).is_ok(),
            ));
        }
        choices.push(VideoChoice::new(
            "Custom…",
            VideoAction::FpsEditor,
            false,
            self.model.snapshot.capabilities.fixed_fps
                && self
                    .video_context()
                    .as_ref()
                    .and_then(|c| self.video.capabilities_for(c))
                    .is_some(),
        ));
        VideoSettingRow::new(
            "Frame rate",
            self.video_dropdown(
                "video-fps",
                "Frame rate",
                match request.fps_mode {
                    FpsMode::AutoMatchSource => native_label,
                    FpsMode::Fixed(f) => format!("{f} FPS"),
                },
                choices,
                editable,
                cx,
            )
            .fill_width()
            .native(native.is_some() && request.fps_mode == FpsMode::AutoMatchSource),
        )
    }
    fn video_inline_number(&self, editor: &NumberEditor, cx: &mut Context<Self>) -> AnyElement {
        let mut row = gpui::div()
            .id("video-inline-number")
            .debug_selector(|| "video-inline-number".into())
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap(gpui::px(4.))
            .bg(crate::ui::theme::colors::Color::RowSurface.get())
            .rounded(gpui::px(6.));
        for (i, (_, input)) in editor.fields.iter().enumerate() {
            if i > 0 {
                row = row.child("×");
            }
            row = row.child(gpui::div().flex_1().min_w_0().child(input.clone()));
        }
        row.on_key_down(cx.listener(|home, event: &gpui::KeyDownEvent, window, cx| {
            if event.keystroke.key == "enter" && !event.is_held {
                home.apply_number(window, cx);
                cx.stop_propagation();
            }
        }))
        .into_any_element()
    }
    fn video_quality(
        &self,
        request: &RecordingRequest,
        caps: Option<&VideoCapabilities>,
        plan: Option<&RecordingPlan>,
        editable: bool,
        cx: &mut Context<Self>,
    ) -> VideoSettingRow {
        let automatic = self.model.automatic_quality();
        let lossless = request
            .optimization
            .as_ref()
            .is_some_and(|o| o.preset == RecordingPreset::Lossless);
        let mut choices = vec![
            VideoChoice::new(
                if automatic {
                    automatic_quality(plan)
                } else {
                    "Automatic".into()
                },
                VideoAction::Edit(VideoEdit::Quality(None)),
                automatic,
                caps.is_some() && !lossless,
            )
            .description("Uses the recording profile and available measured capacity."),
        ];
        for (quality, description) in [
            (Quality::Standard, "CBR with low-latency tuning"),
            (Quality::High, "VBR at a higher bitrate target"),
            (Quality::Maximum, "VBR at the highest bitrate target"),
        ] {
            choices.push(
                VideoChoice::new(
                    format!("{quality:?}"),
                    VideoAction::Edit(VideoEdit::Quality(Some(quality))),
                    !automatic && request.quality == quality,
                    !lossless && caps.is_some_and(|c| c.quality_supported(request, quality)),
                )
                .description(description),
            );
        }
        VideoSettingRow::new(
            "Recording quality",
            self.video_dropdown(
                "video-quality",
                "Recording quality",
                if lossless {
                    "Lossless · QP 0".into()
                } else if automatic {
                    automatic_quality(plan)
                } else {
                    format!("{:?}", request.quality)
                },
                choices,
                editable && caps.is_some() && !lossless,
                cx,
            )
            .fill_width(),
        )
    }
    fn video_codec(
        &self,
        request: &RecordingRequest,
        caps: Option<&VideoCapabilities>,
        plan: Option<&RecordingPlan>,
        editable: bool,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> VideoSettingRow {
        let current = request
            .optimization
            .as_ref()
            .map_or(RecordingCodec::Auto, |o| o.codec);
        let choices = [
            (RecordingCodec::Auto, "Automatic"),
            (RecordingCodec::H264, "H.264"),
            (RecordingCodec::Hevc, "HEVC Main10"),
            (RecordingCodec::Av1, "AV1"),
        ]
        .map(|(codec, label)| {
            let enabled = caps.is_some_and(|c| {
                c.encoders.iter().any(|e| {
                    (codec == RecordingCodec::Auto
                        || crate::video::adapter::codec_name(codec) == e.candidate.codec)
                        && match &request.encoder {
                            EncoderPreference::Auto => true,
                            EncoderPreference::Explicit(id) => id == &e.candidate.id,
                        }
                })
            });
            VideoChoice::new(
                if codec == RecordingCodec::Auto && current == RecordingCodec::Auto {
                    automatic_codec(plan)
                } else {
                    label.into()
                },
                VideoAction::Edit(VideoEdit::Codec(codec)),
                current == codec,
                enabled,
            )
        })
        .to_vec();
        let value = match current {
            RecordingCodec::Auto => automatic_codec(plan),
            RecordingCodec::H264 => "H.264".into(),
            RecordingCodec::Hevc => "HEVC Main10".into(),
            RecordingCodec::Av1 => "AV1".into(),
        };
        VideoSettingRow::new(
            label,
            self.video_dropdown(
                "video-codec",
                label,
                value,
                choices,
                editable && caps.is_some(),
                cx,
            )
            .fill_width(),
        )
    }
    fn video_encoder(
        &self,
        request: &RecordingRequest,
        caps: Option<&VideoCapabilities>,
        plan: Option<&RecordingPlan>,
        editable: bool,
        cx: &mut Context<Self>,
    ) -> VideoSettingRow {
        let codec = request
            .optimization
            .as_ref()
            .map_or(RecordingCodec::Auto, |o| o.codec);
        let mut choices = vec![VideoChoice::new(
            if request.encoder == EncoderPreference::Auto {
                automatic_encoder(caps, plan)
            } else {
                "Automatic hardware encoder".into()
            },
            VideoAction::Edit(VideoEdit::Encoder(EncoderPreference::Auto)),
            request.encoder == EncoderPreference::Auto,
            caps.is_some(),
        )];
        let mut value = match &request.encoder {
            EncoderPreference::Auto => automatic_encoder(caps, plan),
            EncoderPreference::Explicit(id) => format!("Saved · {id}"),
        };
        if let Some(caps) = caps {
            for encoder in &caps.encoders {
                let selected =
                    request.encoder == EncoderPreference::Explicit(encoder.candidate.id.clone());
                if selected {
                    value = encoder.label.clone();
                }
                choices.push(
                    VideoChoice::new(
                        encoder.label.clone(),
                        VideoAction::Edit(VideoEdit::Encoder(EncoderPreference::Explicit(
                            encoder.candidate.id.clone(),
                        ))),
                        selected,
                        codec == RecordingCodec::Auto
                            || crate::video::adapter::codec_name(codec) == encoder.candidate.codec,
                    )
                    .description(caps.adapter.clone()),
                );
            }
        }
        VideoSettingRow::new(
            "Hardware encoder",
            self.video_dropdown(
                "video-encoder",
                "Hardware encoder",
                value,
                choices,
                editable && caps.is_some(),
                cx,
            )
            .fill_width(),
        )
        .description("Uses the capture GPU.")
    }
    #[allow(clippy::too_many_arguments)]
    fn video_native_control(
        &self,
        id: &'static str,
        label: &'static str,
        control: crate::video::adapter::NativeControl,
        request: &RecordingRequest,
        caps: Option<&VideoCapabilities>,
        editable: bool,
        cx: &mut Context<Self>,
    ) -> VideoSettingRow {
        let current = request
            .optimization
            .as_ref()
            .map(|o| o.encoder_overrides)
            .unwrap_or_default();
        let choices: Vec<VideoChoice> =
            crate::video::adapter::native_control_choices(current, control)
                .into_iter()
                .map(|(label, value)| {
                    let enabled = caps.is_some_and(|c| {
                        c.eligible(request)
                            .any(|e| e.native_options.contains(&value))
                    });
                    VideoChoice::new(
                        label,
                        VideoAction::Edit(VideoEdit::EncoderOverrides(value)),
                        value == current,
                        enabled,
                    )
                })
                .collect();
        let value = match control {
            crate::video::adapter::NativeControl::Rate => {
                current.rate_control.map(|v| format!("{v:?}"))
            }
            crate::video::adapter::NativeControl::Qp => current.qp.map(|v| format!("QP {v}")),
            crate::video::adapter::NativeControl::Preset => {
                current.preset.map(|v| format!("{v:?}"))
            }
            crate::video::adapter::NativeControl::Tuning => {
                current.tuning.map(|v| format!("{v:?}"))
            }
            crate::video::adapter::NativeControl::Gop => {
                current.gop_frames.map(|v| format!("{v} frames"))
            }
            crate::video::adapter::NativeControl::Multipass => {
                current.multipass.map(|v| format!("{v:?}"))
            }
            crate::video::adapter::NativeControl::SpatialAq => current
                .spatial_aq
                .map(|v| if v { "On" } else { "Off" }.into()),
            crate::video::adapter::NativeControl::TemporalAq => current
                .temporal_aq
                .map(|v| if v { "On" } else { "Off" }.into()),
        }
        .unwrap_or_else(|| "Automatic policy".into());
        VideoSettingRow::new(
            label,
            self.video_dropdown(
                id,
                label,
                value,
                choices.clone(),
                editable && choices.iter().any(|c| c.enabled),
                cx,
            ),
        )
    }
    fn video_fixed(&self, label: &'static str, value: impl Into<String>) -> VideoSettingRow {
        let debug = format!("video-read-only-{label}");
        VideoSettingRow::new(
            label,
            gpui::div()
                .debug_selector(move || debug.clone())
                .min_w_0()
                .whitespace_normal()
                .child(
                    Text::new(value.into())
                        .variant(TextVariant::Caption)
                        .color(crate::ui::theme::colors::Color::SecondaryText),
                ),
        )
    }
    fn video_section(
        &self,
        section: VideoSection,
        request: &RecordingRequest,
        caps: Option<&VideoCapabilities>,
        plan: Option<&RecordingPlan>,
        editable: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::video::adapter::NativeControl;
        let mut content = Stack::vertical().gap(Spacing::None);
        let options = request.optimization.as_ref();
        match section {
            VideoSection::Geometry => {
                content = content.child(self.video_capture_pair(request, editable, cx));
            }
            VideoSection::Encoder => {
                content = content
                    .child(self.video_encoder(request, caps, plan, editable, cx))
                    .child(self.video_codec(request, caps, plan, editable, "Codec", cx))
                    .child(self.video_quality(request, caps, plan, editable, cx));
            }
            VideoSection::Rate => {
                let lossless = options.is_some_and(|o| o.preset == RecordingPreset::Lossless);
                let cqp = editable
                    && options.is_some_and(|o| {
                        o.encoder_overrides.rate_control == Some(VideoRateControl::Cqp)
                    });
                content = content
                    .child(self.video_native_control("video-rate", "Rate control", NativeControl::Rate,
                        request, caps, editable && !lossless, cx))
                    .child(VideoSettingRow::new("Constant QP", gpui::div()
                        .w_full().min_w_0().flex().flex_wrap().items_center().justify_end().gap(gpui::px(8.))
                        .child(self.video_native_control("video-qp", "Constant QP", NativeControl::Qp,
                            request, caps, cqp, cx).into_control())
                        .child(Button::new("video-qp-custom", "Custom…").size(ButtonSize::Compact)
                            .disabled(!cqp || caps.is_none())
                            .on_click(self.handler(Action::Video(VideoAction::QpEditor), cx))))
                        .description("CQP only; clear the target bitrate first. QP 0 alone is not a lossless profile."))
                    .child(VideoSettingRow::new("Target bitrate", Button::new("video-bitrate",
                        options.and_then(|o| o.bitrate_bps).map(|b| format!("{b} bit/s"))
                            .unwrap_or_else(|| "Automatic…".into()))
                        .size(ButtonSize::Compact)
                        .disabled(!editable || caps.is_none() || (lossless && options.is_none_or(|o| o.bitrate_bps.is_none())))
                        .on_click(self.handler(Action::Video(VideoAction::BitrateEditor), cx))))
                    .child(VideoSettingRow::new("Encoder policy", Button::new("video-reset-native", "Restore automatic")
                        .size(ButtonSize::Compact).disabled(!editable)
                        .on_click(self.handler(Action::Video(VideoAction::Edit(VideoEdit::EncoderOverrides(Default::default()))), cx))));
            }
            VideoSection::Preset => {
                let profile = options.map_or(RecordingPreset::Custom, |o| o.preset);
                let choices = [
                    RecordingPreset::Desktop,
                    RecordingPreset::Gaming,
                    RecordingPreset::Custom,
                    RecordingPreset::Lossless,
                ]
                .map(|p| {
                    VideoChoice::new(
                        format!("{p:?}"),
                        VideoAction::Edit(VideoEdit::Profile(p)),
                        profile == p,
                        caps.is_some()
                            && (p != RecordingPreset::Lossless
                                || caps.is_some_and(|c| c.lossless_supported(request))),
                    )
                })
                .to_vec();
                content = content
                    .child(VideoSettingRow::new(
                        "Recording profile",
                        self.video_dropdown(
                            "video-profile",
                            "Recording profile",
                            format!("{profile:?}"),
                            choices,
                            editable && caps.is_some(),
                            cx,
                        ),
                    ))
                    .child(self.video_native_control(
                        "video-native-preset",
                        "Encoder preset",
                        NativeControl::Preset,
                        request,
                        caps,
                        editable,
                        cx,
                    ))
                    .child(self.video_native_control(
                        "video-native-tuning",
                        "Encoder tuning",
                        NativeControl::Tuning,
                        request,
                        caps,
                        editable,
                        cx,
                    ))
                    .child(VideoSettingRow::new(
                        "Keyframe interval",
                        gpui::div()
                            .w_full()
                            .min_w_0()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .justify_end()
                            .gap(gpui::px(8.))
                            .child(
                                self.video_native_control(
                                    "video-gop",
                                    "Keyframe interval",
                                    NativeControl::Gop,
                                    request,
                                    caps,
                                    editable,
                                    cx,
                                )
                                .into_control(),
                            )
                            .child(
                                Button::new("video-gop-custom", "Custom…")
                                    .size(ButtonSize::Compact)
                                    .disabled(!editable || caps.is_none())
                                    .on_click(
                                        self.handler(Action::Video(VideoAction::GopEditor), cx),
                                    ),
                            ),
                    ));
            }
            VideoSection::Analysis => {
                content = content
                    .child(self.video_native_control(
                        "video-multipass",
                        "Multipass",
                        NativeControl::Multipass,
                        request,
                        caps,
                        editable,
                        cx,
                    ))
                    .child(self.video_native_control(
                        "video-spatial-aq",
                        "Spatial AQ",
                        NativeControl::SpatialAq,
                        request,
                        caps,
                        editable,
                        cx,
                    ))
                    .child(self.video_native_control(
                        "video-temporal-aq",
                        "Temporal AQ",
                        NativeControl::TemporalAq,
                        request,
                        caps,
                        editable,
                        cx,
                    ));
            }
            VideoSection::Color => {
                if !cfg!(target_os = "windows") {
                    return Text::new("Color details are unavailable on this platform.")
                        .variant(TextVariant::Muted)
                        .into_any_element();
                }
                if let Some(context) = self.video_context() {
                    content = content.child(self.video_fixed(
                        "Source dynamic range",
                        if context.hdr { "HDR" } else { "SDR" },
                    ));
                    if plan.is_some() {
                        let lossless =
                            options.is_some_and(|o| o.preset == RecordingPreset::Lossless);
                        content = content
                            .child(self.video_fixed(
                                "Color space / range",
                                if context.hdr {
                                    "BT.2020 / PQ · limited"
                                } else {
                                    "BT.709 · limited"
                                },
                            ))
                            .child(self.video_fixed(
                                "Encoder pixel format",
                                if lossless {
                                    "YUV 4:4:4"
                                } else if context.hdr {
                                    "10-bit 4:2:0"
                                } else {
                                    "8-bit 4:2:0"
                                },
                            ))
                            .child(self.video_fixed(
                                "Bit depth",
                                if context.hdr { "10-bit" } else { "8-bit" },
                            ));
                    }
                }
            }
            VideoSection::Capture => {
                if !cfg!(target_os = "windows") {
                    return Text::new("Capture details are unavailable on this platform.")
                        .variant(TextVariant::Muted)
                        .into_any_element();
                }
                content = content
                    .child(self.video_fixed(
                        "Capture backend",
                        if matches!(request.target, CaptureTarget::Window(_)) {
                            "Windows Graphics Capture"
                        } else {
                            "DXGI Desktop Duplication"
                        },
                    ))
                    .child(
                        self.video_fixed(
                            "Capture adapter",
                            caps.map(|c| c.adapter.clone())
                                .unwrap_or_else(|| "Not checked".into()),
                        ),
                    )
                    .child(self.video_fixed("GPU transport", "Backend managed"))
                    .child(
                        Text::new("Planned values; recording metadata reports the actual session.")
                            .variant(TextVariant::Muted),
                    );
            }
        }
        content.into_any_element()
    }
}
