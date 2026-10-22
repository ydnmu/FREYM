use super::*;
use crate::{
    ui::{
        components::{SegmentedControl, video_settings::*},
        primitives::*,
        theme::spacing::Spacing,
    },
    video::{
        VideoAction, VideoEdit, VideoMode, VideoSection,
        adapter::{EncoderBackend, VideoCapabilities},
    },
};

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
            .disabled(!enabled)
            .on_select(cx.listener(|this, action: &VideoAction, window, cx| {
                this.dispatch(Action::Video(action.clone()), window, cx)
            }))
    }
    pub(super) fn video_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut content = Stack::vertical()
            .gap(Spacing::Xs)
            .child(Text::new("Video").variant(TextVariant::Heading))
            .child(
                SegmentedControl::new()
                    .id("video-mode-switch")
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
                                    .on_click(
                                        self.handler(Action::Video(VideoAction::Mode(mode)), cx),
                                    )
                            },
                        ),
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
        if self.video.mode == VideoMode::Easy {
            content = content
                .child(self.video_resolution(request, editable, cx))
                .child(self.video_fps(request, editable, cx))
                .child(self.video_quality(request, caps, editable, cx))
                .child(self.video_codec(request, caps, editable, "Video format", cx))
                .child(self.video_encoder(request, caps, editable, cx));
        } else {
            for section in VideoSection::ALL {
                let (summary, controls) = self.video_section(section, request, caps, editable, cx);
                content = content.child(
                    VideoDisclosure::new(
                        format!("video-section-{section:?}"),
                        section.title(),
                        summary,
                        controls,
                        self.video.expanded[section.index()],
                    )
                    .on_click(self.handler(Action::Video(VideoAction::Expand(section)), cx)),
                );
            }
        }
        let status = if self.video.probing.is_some() {
            "Checking hardware…".into()
        } else if let Some(error) = &self.video.probe_error {
            error.clone()
        } else if let Some(caps) = caps {
            format!(
                "{} · {} qualified encoder(s). Recording revalidates the selected settings.",
                caps.adapter,
                caps.encoders.len()
            )
        } else {
            "Check hardware to show encoders qualified for this source and frame rate.".into()
        };
        content = content
            .child(Text::new(status).variant(TextVariant::Muted))
            .child(
                Button::new("video-check-hardware", "Check hardware")
                    .size(ButtonSize::Compact)
                    .icon(Icons::Refresh)
                    .disabled(!editable || self.video.probing.is_some())
                    .on_click(self.handler(Action::Video(VideoAction::Refresh), cx)),
            );
        if let Some(caps) = caps {
            match caps.plan(request) {
                Ok(plan) => {
                    content = content.child(Text::new(format!("Plan estimate: {} · {:?} · {} bit/s. Automatic selection may adapt to measured capacity.",
                        plan.encoder.codec.to_uppercase(), plan.rate_control, plan.bitrate_bps)).variant(TextVariant::Muted));
                    if plan.decisions.iter().any(|d| {
                        d.code == frame_settings::policy::DecisionCode::ThroughputPresetSelected
                    }) {
                        content = content.child(Text::new("At 120 FPS and above, the engine uses the low-latency preset while retaining the selected bitrate tier.").variant(TextVariant::Muted));
                    }
                }
                Err(error) => {
                    content = content.child(
                        Text::new(format!("Selection needs attention: {error}"))
                            .variant(TextVariant::Muted),
                    )
                }
            }
        }
        content.into_any_element()
    }
    fn video_resolution(
        &self,
        request: &RecordingRequest,
        editable: bool,
        cx: &mut Context<Self>,
    ) -> VideoSettingRow {
        let value = match request.resolution {
            Resolution::Native => self
                .video_context()
                .and_then(|c| {
                    c.dimensions.or_else(|| {
                        self.video
                            .capabilities_for(&c)
                            .map(|caps| caps.native_dimensions)
                    })
                })
                .map(|d| format!("Native · {} × {}", d.width, d.height))
                .unwrap_or_else(|| "Native · source size".into()),
            Resolution::Exact(d) => format!("Saved · {} × {}", d.width, d.height),
        };
        VideoSettingRow::new(
            "Resolution",
            self.video_dropdown(
                "video-resolution",
                "Resolution",
                value,
                vec![VideoChoice::new(
                    "Native source resolution",
                    VideoAction::Edit(VideoEdit::NativeResolution),
                    request.resolution == Resolution::Native,
                    true,
                )],
                editable,
                cx,
            ),
        )
        .description("Native source size. Output scaling is unavailable.")
    }
    fn video_fps(
        &self,
        request: &RecordingRequest,
        editable: bool,
        cx: &mut Context<Self>,
    ) -> VideoSettingRow {
        let mut choices = vec![
            VideoChoice::new(
                "Match source",
                VideoAction::Edit(VideoEdit::Fps(None)),
                request.fps_mode == FpsMode::AutoMatchSource,
                true,
            )
            .description("Keeps real source updates; no duplicate frames."),
        ];
        for fps in [24, 30, 60, 120] {
            choices.push(VideoChoice::new(
                format!("{fps} FPS"),
                VideoAction::Edit(VideoEdit::Fps(Some(fps * 1000))),
                request.fps_mode == FpsMode::Fixed(f64::from(fps)),
                self.model.snapshot.capabilities.fixed_fps,
            ));
        }
        choices.push(
            VideoChoice::new(
                "Custom frame rate…",
                VideoAction::FpsEditor,
                false,
                self.model.snapshot.capabilities.fixed_fps,
            )
            .description(
                "Accepts fractional and higher rates; source pace still limits real frames.",
            ),
        );
        VideoSettingRow::new(
            "Frame rate",
            self.video_dropdown(
                "video-fps",
                "Frame rate",
                match request.fps_mode {
                    FpsMode::AutoMatchSource => "Match source".into(),
                    FpsMode::Fixed(fps) => format!("{fps} FPS"),
                },
                choices,
                editable,
                cx,
            ),
        )
    }
    fn video_quality(
        &self,
        request: &RecordingRequest,
        caps: Option<&VideoCapabilities>,
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
                "Automatic",
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
                    "Automatic".into()
                } else {
                    format!("{:?}", request.quality)
                },
                choices,
                editable && caps.is_some() && !lossless,
                cx,
            ),
        )
    }
    fn video_codec(
        &self,
        request: &RecordingRequest,
        caps: Option<&VideoCapabilities>,
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
                label,
                VideoAction::Edit(VideoEdit::Codec(codec)),
                current == codec,
                enabled,
            )
        })
        .to_vec();
        let value = match current {
            RecordingCodec::Auto => "Automatic",
            RecordingCodec::H264 => "H.264",
            RecordingCodec::Hevc => "HEVC Main10",
            RecordingCodec::Av1 => "AV1",
        };
        VideoSettingRow::new(
            label,
            self.video_dropdown(
                "video-codec",
                label,
                value.into(),
                choices,
                editable && caps.is_some(),
                cx,
            ),
        )
        .description("MKV container. Codecs depend on the source and hardware.")
    }
    fn video_encoder(
        &self,
        request: &RecordingRequest,
        caps: Option<&VideoCapabilities>,
        editable: bool,
        cx: &mut Context<Self>,
    ) -> VideoSettingRow {
        let codec = request
            .optimization
            .as_ref()
            .map_or(RecordingCodec::Auto, |o| o.codec);
        let mut choices = vec![VideoChoice::new(
            "Automatic hardware encoder",
            VideoAction::Edit(VideoEdit::Encoder(EncoderPreference::Auto)),
            request.encoder == EncoderPreference::Auto,
            caps.is_some(),
        )];
        let mut value = match &request.encoder {
            EncoderPreference::Auto => "Automatic".into(),
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
            ),
        )
        .description("Capture GPU only. Unavailable saved selections stay selected.")
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
        let choices = crate::video::adapter::native_control_choices(current, control)
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
            self.video_dropdown(id, label, value, choices, editable && caps.is_some(), cx),
        )
    }
    fn video_fixed(
        &self,
        label: &'static str,
        value: impl Into<String>,
        description: &'static str,
    ) -> VideoSettingRow {
        VideoSettingRow::new(label, Text::new(value.into()).variant(TextVariant::Caption))
            .description(description)
    }
    fn video_section(
        &self,
        section: VideoSection,
        request: &RecordingRequest,
        caps: Option<&VideoCapabilities>,
        editable: bool,
        cx: &mut Context<Self>,
    ) -> (String, AnyElement) {
        let mut content = Stack::vertical().gap(Spacing::Xxs);
        let plan = caps.and_then(|c| c.plan(request).ok());
        let options = request.optimization.as_ref();
        let summary = match section {
            VideoSection::Geometry => {
                content = content.child(self.video_resolution(request, editable, cx))
                    .child(self.video_fixed("Scaling filter", "Unavailable", "The encoder accepts native geometry; a scaling filter would have no effect."))
                    .child(self.video_fps(request, editable, cx));
                match request.fps_mode {
                    FpsMode::AutoMatchSource => "Native · source paced".into(),
                    FpsMode::Fixed(fps) => format!("{fps} FPS · native"),
                }
            }
            VideoSection::Encoder => {
                content = content
                    .child(self.video_encoder(request, caps, editable, cx))
                    .child(self.video_codec(request, caps, editable, "Codec", cx))
                    .child(self.video_quality(request, caps, editable, cx));
                plan.as_ref()
                    .map(|p| p.encoder.codec.to_uppercase())
                    .unwrap_or_else(|| "Hardware qualified choices".into())
            }
            VideoSection::Rate => {
                let lossless = options.is_some_and(|o| o.preset == RecordingPreset::Lossless);
                let rate = if lossless {
                    "Lossless QP 0".into()
                } else {
                    plan.as_ref()
                        .map(|p| format!("{:?}", p.rate_control).to_uppercase())
                        .unwrap_or_else(|| "Not qualified".into())
                };
                content = content.child(self.video_native_control("video-rate", "Rate control", crate::video::adapter::NativeControl::Rate, request, caps, editable && !lossless, cx)
                    .description("Independent NVENC modes. ICQ and CRF are unavailable on this encoder path."))
                    .child(Button::new("video-reset-native", "Restore automatic encoder policy")
                        .size(ButtonSize::Compact).disabled(!editable)
                        .on_click(self.handler(Action::Video(VideoAction::Edit(VideoEdit::EncoderOverrides(Default::default()))), cx)))
                    .child(self.video_native_control("video-qp", "Constant QP", crate::video::adapter::NativeControl::Qp, request, caps, editable && options.is_some_and(|o| o.encoder_overrides.rate_control == Some(VideoRateControl::Cqp)), cx)
                    .description("CQP only. QP 0 is not the separately qualified lossless profile. Clear target bitrate before choosing CQP."))
                    .child(Button::new("video-qp-custom", "Custom QP…").size(ButtonSize::Compact)
                        .disabled(!editable || caps.is_none() || options.is_none_or(|o| o.encoder_overrides.rate_control != Some(VideoRateControl::Cqp)))
                        .on_click(self.handler(Action::Video(VideoAction::QpEditor), cx)))
                    .child(VideoSettingRow::new("Target bitrate", Button::new("video-bitrate", options.and_then(|o| o.bitrate_bps).map(|b| format!("{b} bit/s")).unwrap_or_else(|| "Automatic…".into()))
                        .size(ButtonSize::Compact).disabled(!editable || caps.is_none() || (lossless && options.is_none_or(|o| o.bitrate_bps.is_none())))
                        .on_click(self.handler(Action::Video(VideoAction::BitrateEditor), cx)))
                        .description("Enter an exact bit/s target. The codec level limit is validated before this Video editor saves it."))
                    .child(self.video_fixed("Peak bitrate", "Engine derived", "VBR peak follows the target and codec ceiling; independent peak/VBV controls are unavailable."));
                rate
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
                content = content.child(VideoSettingRow::new("Recording profile", self.video_dropdown("video-profile", "Recording profile", format!("{profile:?}"), choices, editable && caps.is_some(), cx)))
                    .child(self.video_native_control("video-native-preset", "Encoder preset", crate::video::adapter::NativeControl::Preset, request, caps, editable, cx))
                    .child(self.video_native_control("video-native-tuning", "Encoder tuning", crate::video::adapter::NativeControl::Tuning, request, caps, editable, cx)
                        .description("Explicit choices override the automatic high-FPS throughput policy and may reduce throughput."))
                    .child(self.video_native_control("video-gop", "Keyframe interval", crate::video::adapter::NativeControl::Gop, request, caps, editable, cx))
                    .child(Button::new("video-gop-custom", "Custom keyframe interval…").size(ButtonSize::Compact)
                        .disabled(!editable || caps.is_none())
                        .on_click(self.handler(Action::Video(VideoAction::GopEditor), cx)))
                    .child(self.video_fixed("B-frames", "0 on the current Windows paths", "Frame reordering is disabled. No editable B-frame count is implemented."));
                format!("{profile:?}")
            }
            VideoSection::Analysis => {
                content = content
                    .child(self.video_native_control(
                        "video-multipass",
                        "Multipass",
                        crate::video::adapter::NativeControl::Multipass,
                        request,
                        caps,
                        editable,
                        cx,
                    ))
                    .child(self.video_fixed(
                        "Lookahead",
                        "Unavailable · 0 frames",
                        "The four-surface owner cannot admit delayed lookahead output.",
                    ))
                    .child(self.video_native_control(
                        "video-spatial-aq",
                        "Spatial AQ",
                        crate::video::adapter::NativeControl::SpatialAq,
                        request,
                        caps,
                        editable,
                        cx,
                    ))
                    .child(self.video_native_control(
                        "video-temporal-aq",
                        "Temporal AQ",
                        crate::video::adapter::NativeControl::TemporalAq,
                        request,
                        caps,
                        editable,
                        cx,
                    ));
                "Qualified native controls".into()
            }

            VideoSection::Color => {
                if !cfg!(target_os = "windows") {
                    return ("Platform capability provider required".into(), Text::new("This platform adapter does not publish color, pixel format, HDR or bit-depth capabilities yet.").variant(TextVariant::Muted).into_any_element());
                }
                let hdr = self.video_context().is_some_and(|c| c.hdr);
                let lossless = options.is_some_and(|o| o.preset == RecordingPreset::Lossless);
                content = content.child(self.video_fixed("Dynamic range", if hdr { "HDR source · Main10 required" } else { "SDR source" },
                        "Follows the active source signal. SDR/HDR conversion and tone mapping are unavailable."))
                    .child(self.video_fixed("Color space / range", if hdr { "BT.2020 / PQ · limited" } else { "BT.709 · limited" },
                        "Windows encoder path definition; actual session metadata remains authoritative. No manual matrix or range override."))
                    .child(self.video_fixed("Encoder pixel format", if lossless { "YUV 4:4:4" } else if hdr { "10-bit 4:2:0" } else { "8-bit 4:2:0" },
                        "NVENC converts BGRA8 or FP16 scRGB on the GPU. MF input transport is backend managed; pixel format is not selectable."))
                    .child(self.video_fixed("Bit depth", if hdr { "10-bit output" } else { "8-bit output" }, "10-bit output requires an active HDR source and a qualified Main10 encoder."));
                if hdr {
                    "HDR source · 10-bit path".into()
                } else {
                    "SDR source · 8-bit path".into()
                }
            }
            VideoSection::Capture => {
                if !cfg!(target_os = "windows") {
                    return ("Platform capability provider required".into(), Text::new("This platform adapter does not publish capture-backend or GPU controls yet.").variant(TextVariant::Muted).into_any_element());
                }
                let backend = if matches!(request.target, CaptureTarget::Window(_)) {
                    "Windows Graphics Capture"
                } else {
                    "DXGI Desktop Duplication"
                };
                content = content.child(self.video_fixed("Capture backend", backend, "Selected by the source type. Manual backend override is unavailable."))
                    .child(self.video_fixed("Capture adapter", caps.map(|c| c.adapter.clone()).unwrap_or_else(|| "Not checked".into()), "Capture and encoding use the same adapter. Cross-GPU selection is unavailable."))
                    .child(self.video_fixed("GPU transport", if caps.is_some_and(|c| c.eligible(request).any(|e| e.backend == EncoderBackend::MediaFoundation)) {
                        "Backend managed · MF defaults to CPU transfer" } else { "Backend managed" },
                        "MF shared GPU/NV12 paths are diagnostic environment options; this UI does not change process-wide flags."))
                    .child(VideoSettingRow::new("Show mouse cursor", self.toggle(Action::Cursor, "Show mouse cursor", request.show_cursor, editable, cx)));
                backend.into()
            }
        };
        (summary, content.into_any_element())
    }
}
