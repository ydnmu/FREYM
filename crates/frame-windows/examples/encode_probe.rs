//! Diagnostic Windows hardware H.264 encode proof (FRAME-0104, quality policy FRAME-0207).
//!
//! It captures one real display through the accepted FRAME-0103 session, encodes the
//! captured surfaces with the hardware H.264 encoder, and writes exactly two new
//! artifacts: an Annex-B elementary stream and a streaming diagnostic JSONL sidecar.
//! It is a bounded proof, not a recorder: there is no container, no mux, no audio, no
//! index, no recovery journal and no UI binding, and it never overwrites evidence.
//!
//! Since FRAME-0207 the example is also the real consumer of the recording policy: it
//! qualifies the local encoders, builds one explicit plan for the requested quality and
//! frame-rate selection, applies that plan to the encoder configuration, and admits real
//! captured frames through the source-subsampling helper. The sidecar records the plan the
//! run applied and the admission counters, and `--settings PATH` exercises the versioned
//! settings document on disk.
//!
//! ```text
//! cargo run --locked -p frame-windows --example encode_probe -- \
//!     --display-id "\\?\DISPLAY#BOE0D5E#5&12ac677&0&UID8448#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}" \
//!     --seconds 30 \
//!     --output test-results/frame-0104-30s.h264 \
//!     --rate-num 180 --rate-den 1
//! ```
//!
//! Options: `--display-id ID` (required), `--seconds N` (1..=3600, default 30),
//! `--output PATH` (required, must not exist), `--rate-num N` and `--rate-den N`
//! (required for source-paced recording: the explicit nominal encoder rate),
//! `--quality standard|high|maximum`, `--fps-mode auto|fixed`, `--fps N` (required with
//! `--fps-mode fixed`: real captured frames are admitted on that grid), `--encoder ID`
//! (an explicit encoder instead of automatic selection) and `--settings PATH` (a versioned
//! settings document to read and rewrite). The source nominal rate recorded in the sidecar
//! is the enumerated nominal mode rate of the selected display; it is not a measurement and
//! is never presented as the encoded rate.

#[cfg(target_os = "windows")]
fn main() -> std::process::ExitCode {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--frame-mf-helper")
    {
        std::process::exit(frame_windows::run_mf_helper());
    }
    probe::run()
}

#[cfg(not(target_os = "windows"))]
fn main() -> std::process::ExitCode {
    eprintln!("Native hardware encoding is implemented for Windows only.");
    std::process::ExitCode::FAILURE
}

#[cfg(target_os = "windows")]
mod probe {
    use std::num::NonZeroU32;
    use std::path::{Path, PathBuf};
    use std::process::ExitCode;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use frame_capture::{
        CaptureBackend, CaptureCancellation, CaptureContext, CaptureSession, CapturedFrame,
        DisplayBackend,
    };
    use frame_clock::MasterClock;
    use frame_contracts::{
        CaptureTarget, Dimensions, DisplayId, EncoderPreference, ErrorCode, FpsMode,
        OutputSettings, Quality, RecorderError, RecordingRequest, RecoveryStatus, Resolution,
    };
    use frame_encode::{
        EncoderInput, EncoderPoll, PacketEncoder, SubmitOutcome, admission::FrameAdmission,
        admission::FrameAdmitter, duration_until, nominal_duration,
    };
    use frame_settings::{
        EncoderCandidate, EncoderLimits, EncoderPreset, FrameRatePlan, NominalRate, PlanInputs,
        RateControl, SettingsStore, plan_recording,
    };
    use frame_windows::{
        NativeEncoderConfig, NativeEncoderPreset, NativeQualitySettings, NativeRateControl,
        PlatformBackend, ProofArtifacts, WindowsCaptureSession, WindowsH264Encoder, WindowsSurface,
    };
    use serde::Serialize;

    const USAGE: &str = "usage: encode_probe --display-id ID --output PATH [--rate-num N --rate-den N] [--quality standard|high|maximum] [--fps-mode auto|fixed] [--fps N] [--encoder ID] [--settings PATH] [--seconds 1..3600] [--stall-consumer-ms N]";
    const MAX_SECONDS: u32 = 3600;
    /// The same five-second native-progress budget the encoder worker uses between its
    /// own calls, applied by the proof caller so a driver call that stops completing work
    /// is reported instead of waited out.
    const PROGRESS_WATCHDOG: Duration = Duration::from_secs(5);

    /// The frame-rate selection named on the command line, before a stored selection or an
    /// explicit manual rate is taken into account.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum FpsSelection {
        Auto,
        Fixed,
    }

    struct Options {
        display_id: Option<String>,
        seconds: u32,
        output: Option<PathBuf>,
        rate_num: Option<u32>,
        rate_den: Option<u32>,
        /// The requested quality tier; absent means "use the stored selection or High".
        quality: Option<Quality>,
        /// The requested frame-rate selection, unresolved so a stored one can fill it in.
        fps_mode: Option<FpsSelection>,
        /// An explicit manual rate; it implies a fixed frame-rate selection.
        fps: Option<f64>,
        /// The explicit encoder identity; absent means automatic selection.
        encoder: Option<String>,
        /// A versioned settings document to read and rewrite.
        settings: Option<PathBuf>,
        /// Diagnostic pressure phase: hold the delivery queue for this long, so the
        /// bounded packet credits fill and `submit` must return the owned input.
        stall_consumer_ms: u32,
    }

    fn parse_options() -> Result<Options, String> {
        let arguments: Vec<String> = std::env::args().skip(1).collect();
        let mut options = Options {
            display_id: None,
            seconds: 30,
            output: None,
            rate_num: None,
            rate_den: None,
            quality: None,
            fps_mode: None,
            fps: None,
            encoder: None,
            settings: None,
            stall_consumer_ms: 0,
        };
        let mut index = 0usize;
        while index < arguments.len() {
            let argument = arguments[index].as_str();
            let mut take = |name: &str| -> Result<String, String> {
                index += 1;
                arguments
                    .get(index)
                    .cloned()
                    .ok_or_else(|| format!("{name} needs a value"))
            };
            match argument {
                "--display-id" => options.display_id = Some(take("--display-id")?),
                "--seconds" => {
                    let raw = take("--seconds")?;
                    let seconds: u32 = raw
                        .parse()
                        .map_err(|_| format!("--seconds expects a number, not {raw}"))?;
                    if seconds == 0 || seconds > MAX_SECONDS {
                        return Err(format!(
                            "--seconds must be 1..={MAX_SECONDS}, not {seconds}"
                        ));
                    }
                    options.seconds = seconds;
                }
                "--quality" => {
                    let raw = take("--quality")?;
                    options.quality = Some(match raw.as_str() {
                        "standard" => Quality::Standard,
                        "high" => Quality::High,
                        "maximum" => Quality::Maximum,
                        other => {
                            return Err(format!(
                                "--quality expects standard, high or maximum, not {other}"
                            ));
                        }
                    });
                }
                "--fps-mode" => {
                    let raw = take("--fps-mode")?;
                    options.fps_mode = Some(match raw.as_str() {
                        "auto" => FpsSelection::Auto,
                        "fixed" => FpsSelection::Fixed,
                        other => {
                            return Err(format!("--fps-mode expects auto or fixed, not {other}"));
                        }
                    });
                }
                "--fps" => {
                    let raw = take("--fps")?;
                    let fps: f64 = raw
                        .parse()
                        .map_err(|_| format!("--fps expects a number, not {raw}"))?;
                    options.fps = Some(fps);
                }
                "--encoder" => options.encoder = Some(take("--encoder")?),
                "--settings" => options.settings = Some(PathBuf::from(take("--settings")?)),
                "--stall-consumer-ms" => {
                    let raw = take("--stall-consumer-ms")?;
                    options.stall_consumer_ms = raw
                        .parse()
                        .map_err(|_| format!("--stall-consumer-ms expects a number, not {raw}"))?;
                }
                "--output" => options.output = Some(PathBuf::from(take("--output")?)),
                "--rate-num" => {
                    let raw = take("--rate-num")?;
                    options.rate_num = Some(
                        raw.parse()
                            .map_err(|_| format!("--rate-num expects a number, not {raw}"))?,
                    );
                }
                "--rate-den" => {
                    let raw = take("--rate-den")?;
                    options.rate_den = Some(
                        raw.parse()
                            .map_err(|_| format!("--rate-den expects a number, not {raw}"))?,
                    );
                }
                "--help" | "-h" => {
                    println!("{USAGE}");
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument {other}")),
            }
            index += 1;
        }
        Ok(options)
    }

    #[derive(Serialize)]
    struct SettingsRecord {
        kind: &'static str,
        tool: &'static str,
        task: &'static str,
        display_id: String,
        width: u32,
        height: u32,
        enumerated_source_nominal_hz: Option<f64>,
        nominal_rate_num: u32,
        nominal_rate_den: u32,
        bitrate_bps: u32,
        max_bitrate_bps: u32,
        rate_control: &'static str,
        preset: &'static str,
        gop_frames: u32,
        idr_period: u32,
        frame_interval_p: u32,
        enable_ptd: bool,
        enable_async: bool,
        repeat_sps_pps: bool,
        profile_high: bool,
        vui_full_range: bool,
        caps_preset_available: bool,
        input_format: &'static str,
        codec_config_bytes: usize,
        output_buffers: u32,
        output_buffer_capacity: u32,
        max_access_unit_bytes: u32,
        stream_path: String,
        sidecar_path: String,
        started_unix_ms: u128,
    }

    /// The plan the run applied, as the policy reported it (FRAME-0207).
    ///
    /// The requested values here are configuration, not measurements: the bitrate is the
    /// documented target for the chosen tier and the nominal configuration rate, and the
    /// nominal rate itself is never a measured throughput.
    #[derive(Serialize)]
    struct PlanRecord {
        kind: &'static str,
        encoder_id: String,
        encoder_codec: String,
        encoder_hardware: bool,
        encoder_max_width: u32,
        encoder_max_height: u32,
        quality: &'static str,
        resolution: &'static str,
        source_width: u32,
        source_height: u32,
        frame_rate_mode: &'static str,
        nominal_rate_num: u32,
        nominal_rate_den: u32,
        calculated_bitrate_bps: u32,
        applied_bitrate_bps: u32,
        applied_peak_bitrate_bps: u32,
        level: &'static str,
        level_ceiling_bps: u32,
        gop_frames: u32,
        rate_control: &'static str,
        preset: &'static str,
        decisions: Vec<PlanDecisionRecord>,
    }

    #[derive(Serialize)]
    struct PlanDecisionRecord {
        code: String,
        detail: String,
    }

    /// The selections this run would persist, and what the settings document held.
    #[derive(Serialize)]
    struct SettingsPersistenceRecord {
        kind: &'static str,
        settings_path: Option<String>,
        loaded_from_disk: bool,
        saved: bool,
    }

    #[derive(Serialize)]
    struct PacketRecord {
        kind: &'static str,
        index: u64,
        offset: u64,
        bytes: usize,
        pts_ns: u64,
        dts_ns: u64,
        duration_ns: u64,
        keyframe: bool,
    }

    #[derive(Serialize)]
    struct SummaryRecord {
        kind: &'static str,
        accepted_inputs: u64,
        backpressure_returns: u64,
        native_submissions: u64,
        native_buffered: u64,
        native_failures: u64,
        packets_emitted: u64,
        packets_dequeued: u64,
        packets_written: u64,
        bytes_written: u64,
        packets_discarded_on_stop: u64,
        retained_leases: u64,
        registrations: u64,
        unregistrations: u64,
        eos_submissions: u64,
        eos_acknowledgements: u64,
        native_busy_retries: u64,
        timestamp_mismatches: u64,
        timestamp_regressions: u64,
        drained: bool,
        monotonic_timestamps: bool,
        first_pts_ns: Option<u64>,
        last_pts_ns: Option<u64>,
        first_keyframe: bool,
        initialization_bytes_written: bool,
        duration_s: f64,
        identities_reconciled: bool,
        /// Real captured frames the admission policy saw.
        frames_seen: u64,
        /// Real captured frames that continued to the encoder.
        frames_passed: u64,
        /// Frames the frame-rate policy skipped. A policy counter, never capture loss.
        frames_skipped_by_policy: u64,
    }

    fn serialization_error(error: serde_json::Error) -> RecorderError {
        RecorderError {
            code: ErrorCode::Internal,
            message: format!("Serializing a diagnostic sidecar record failed: {error}"),
            recovery: RecoveryStatus::Unknown,
            action: "This is a defect in the diagnostic sidecar schema.".to_string(),
        }
    }

    /// Lists the enumerated displays, so an operator copies a real identity.
    fn available_displays() -> String {
        match PlatformBackend.enumerate_displays() {
            Ok(displays) => displays
                .iter()
                .map(|display| {
                    format!(
                        "  {} — {}x{} primary={} refresh_rate_hz={:?}\n",
                        display.id.0,
                        display.width,
                        display.height,
                        display.is_primary,
                        display.refresh_rate_hz
                    )
                })
                .collect(),
            Err(error) => format!("  (enumeration failed: {})\n", error.message),
        }
    }

    /// True when one access unit already carries its own SPS and PPS NAL units.
    ///
    /// Annex-B start codes delimit the NAL units; types 7 (SPS) and 8 (PPS) are what a
    /// decoder needs at the head of a stream. This only decides whether the codec
    /// initialization bytes must be written ahead of the first access unit, so a
    /// decodable standalone stream is produced without duplicating parameter sets.
    fn parameter_sets_present(access_unit: &[u8]) -> bool {
        let mut sps = false;
        let mut pps = false;
        let mut index = 0usize;
        while index + 3 < access_unit.len() {
            let start = if access_unit[index..].starts_with(&[0, 0, 0, 1]) {
                index + 4
            } else if access_unit[index..].starts_with(&[0, 0, 1]) {
                index + 3
            } else {
                index += 1;
                continue;
            };
            if start >= access_unit.len() {
                break;
            }
            match access_unit[start] & 0x1F {
                7 => sps = true,
                8 => pps = true,
                _ => {}
            }
            index = start;
        }
        sps && pps
    }

    /// Everything the run counted about the written stream.
    struct Written {
        packets: u64,
        bytes: u64,
        monotonic: bool,
        first_pts: Option<u64>,
        last_pts: Option<u64>,
        first_keyframe: bool,
        initialization_written: bool,
    }

    /// Writes every ready packet to the stream and its sidecar line, in order.
    ///
    /// A write or flush failure fails the run: the artifacts stay on disk explicitly
    /// incomplete and are never reported as a finished proof.
    fn drain_packets<I>(
        encoder: &mut impl PacketEncoder<I>,
        artifacts: &mut ProofArtifacts,
        written: &mut Written,
    ) -> Result<bool, RecorderError> {
        loop {
            match encoder.try_packet()? {
                EncoderPoll::Packet(packet) => {
                    if !written.initialization_written {
                        if !parameter_sets_present(&packet.data) {
                            artifacts.write_initialization(&encoder.stream_info().codec_config)?;
                        }
                        written.initialization_written = true;
                    }
                    let offset = artifacts.write_access_unit(&packet.data)?;
                    if written.packets == 0 {
                        written.first_pts = Some(packet.pts.0);
                        written.first_keyframe = packet.keyframe;
                    } else if let Some(last) = written.last_pts
                        && packet.pts.0 <= last
                    {
                        written.monotonic = false;
                    }
                    written.last_pts = Some(packet.pts.0);
                    written.packets += 1;
                    written.bytes += packet.data.len() as u64;
                    let record = PacketRecord {
                        kind: "packet",
                        index: written.packets - 1,
                        offset,
                        bytes: packet.data.len(),
                        pts_ns: packet.pts.0,
                        dts_ns: packet.dts.0,
                        duration_ns: packet.duration_ns,
                        keyframe: packet.keyframe,
                    };
                    artifacts.write_sidecar_line(&serde_json::to_string(&record).map_err(
                        |error| RecorderError {
                            code: ErrorCode::Internal,
                            message: format!("Serializing a packet record failed: {error}"),
                            recovery: RecoveryStatus::Unknown,
                            action:
                                "This is a defect in the diagnostic sidecar schema.".to_string(),
                        },
                    )?)?;
                }
                EncoderPoll::Pending => return Ok(false),
                EncoderPoll::Drained => return Ok(true),
            }
        }
    }

    /// The selections this run resolved: the stored document's values fill in whatever the
    /// command line left out, so a stored selection is used exactly as it was written.
    struct Selections {
        quality: Quality,
        fps_mode: FpsMode,
        encoder: EncoderPreference,
        resolution: Resolution,
        loaded_from_disk: bool,
    }

    /// Resolves the CLI and the stored document into one set of existing selections.
    fn resolve_selections(
        options: &Options,
    ) -> Result<(Selections, Option<SettingsStore>), RecorderError> {
        let store = options.settings.clone().map(SettingsStore::at);
        let stored = match store.as_ref().map(|store| store.load()) {
            None => None,
            Some(Ok(stored)) => stored,
            Some(Err(error)) => {
                // A stored document that cannot be read is a visible failure: it is never
                // treated as "no settings" and never silently replaced.
                return Err(RecorderError {
                    code: ErrorCode::InvalidRequest,
                    message: format!("The stored recording settings could not be used: {error}"),
                    recovery: RecoveryStatus::NoRecording,
                    action: "Fix or remove the settings document, or start without --settings."
                        .to_string(),
                });
            }
        };
        let loaded_from_disk = stored.is_some();
        let fps_mode = match options.fps {
            Some(fps) => FpsMode::Fixed(fps),
            None => match options.fps_mode {
                Some(FpsSelection::Auto) => FpsMode::AutoMatchSource,
                Some(FpsSelection::Fixed) => {
                    return Err(RecorderError {
                        code: ErrorCode::InvalidRequest,
                        message: "--fps-mode fixed needs --fps with the manual rate.".to_string(),
                        recovery: RecoveryStatus::NoRecording,
                        action: "Add --fps N, or use --fps-mode auto.".to_string(),
                    });
                }
                None => stored
                    .as_ref()
                    .map_or(FpsMode::AutoMatchSource, |stored| stored.fps_mode),
            },
        };
        let selections = Selections {
            quality: options
                .quality
                .or_else(|| stored.as_ref().map(|stored| stored.quality))
                .unwrap_or(Quality::High),
            fps_mode,
            encoder: match &options.encoder {
                Some(id) => EncoderPreference::Explicit(id.clone()),
                None => stored
                    .as_ref()
                    .map_or(EncoderPreference::Auto, |stored| stored.encoder.clone()),
            },
            resolution: stored
                .as_ref()
                .map_or(Resolution::Native, |stored| stored.resolution),
            loaded_from_disk,
        };
        Ok((selections, store))
    }

    pub(super) fn run() -> ExitCode {
        let options = match parse_options() {
            Ok(options) => options,
            Err(message) => {
                eprintln!("encode_probe: {message}");
                eprintln!("{USAGE}");
                return ExitCode::FAILURE;
            }
        };
        let Some(display_id) = options.display_id.clone() else {
            eprintln!("encode_probe: --display-id is required; use an id from a real display.");
            eprintln!("Currently enumerated displays:");
            eprint!("{}", available_displays());
            return ExitCode::FAILURE;
        };
        let Some(output) = options.output.clone() else {
            eprintln!("encode_probe: --output is required and must not already exist.");
            eprintln!("{USAGE}");
            return ExitCode::FAILURE;
        };
        let (selections, store) = match resolve_selections(&options) {
            Ok(resolved) => resolved,
            Err(error) => {
                eprintln!("encode_probe: {}", error.message);
                eprintln!("action: {}", error.action);
                return ExitCode::FAILURE;
            }
        };
        // A source-paced recording is configured with an explicit nominal rate; a manual
        // rate is itself the configuration rate, whether it came from the command line or
        // from the stored document, so it needs no separate nominal rate.
        let nominal_rate = match selections.fps_mode {
            FpsMode::AutoMatchSource => {
                let (Some(rate_num), Some(rate_den)) = (options.rate_num, options.rate_den) else {
                    eprintln!(
                        "encode_probe: --rate-num and --rate-den are required for source-paced recording; the nominal encoder rate is explicit and is never derived from a measured source rate."
                    );
                    return ExitCode::FAILURE;
                };
                let (Some(rate_num), Some(rate_den)) =
                    (NonZeroU32::new(rate_num), NonZeroU32::new(rate_den))
                else {
                    eprintln!("encode_probe: the nominal rate must be a positive rational.");
                    return ExitCode::FAILURE;
                };
                NominalRate::new(rate_num, rate_den)
            }
            FpsMode::Fixed(requested) => match NominalRate::recover_exact(requested) {
                Ok(rate) => rate,
                Err(error) => {
                    eprintln!("encode_probe: {}", error.message);
                    return ExitCode::FAILURE;
                }
            },
        };

        match encode(
            &display_id,
            options.seconds,
            &output,
            nominal_rate,
            &selections,
            store.as_ref(),
            options.stall_consumer_ms,
        ) {
            Ok(report) => {
                print!("{report}");
                ExitCode::SUCCESS
            }
            Err((error, partial)) => {
                eprintln!("encode_probe failed: {:?}", error.code);
                eprintln!("{}", error.message);
                eprintln!("action: {}", error.action);
                for path in partial {
                    eprintln!(
                        "partial artifact (incomplete, not evidence): {}",
                        path.display()
                    );
                }
                ExitCode::FAILURE
            }
        }
    }

    /// The plan this run applies, mapped onto the encoder configuration's own knobs.
    fn encoder_config(
        plan: &frame_settings::RecordingPlan,
    ) -> Result<NativeEncoderConfig, RecorderError> {
        // The only resolution plan this path produces is the capture surface's own size:
        // nothing is scaled here and nothing was silently recorded at another size.
        let frame_settings::ResolutionPlan::Native(native) = plan.resolution;
        NativeEncoderConfig::with_quality(
            native.width,
            native.height,
            plan.nominal_rate.num(),
            plan.nominal_rate.den(),
            NativeQualitySettings {
                bitrate_bps: plan.bitrate_bps,
                peak_bitrate_bps: plan.peak_bitrate_bps,
                gop_frames: plan.gop_frames,
                rate_control: match (plan.preset, plan.rate_control) {
                    (EncoderPreset::Lossless, _) => NativeRateControl::Lossless,
                    (_, RateControl::Cbr) => NativeRateControl::Cbr,
                    (_, RateControl::Vbr) => NativeRateControl::Vbr,
                    (_, RateControl::Cqp) => NativeRateControl::Cqp,
                },
                preset: match plan.preset {
                    EncoderPreset::Lossless => NativeEncoderPreset::Lossless,
                    EncoderPreset::LowLatency => NativeEncoderPreset::LowLatency,
                    EncoderPreset::HighQuality => NativeEncoderPreset::HighQuality,
                    EncoderPreset::MaximumQuality => NativeEncoderPreset::MaximumQuality,
                },
            },
        )
    }

    /// Builds the plan for this run from real candidate qualification.
    fn build_plan(
        display: &frame_contracts::DisplayInfo,
        selections: &Selections,
        nominal_rate: NominalRate,
    ) -> Result<frame_settings::RecordingPlan, RecorderError> {
        let candidates = frame_windows::probe_native_encoders()?;
        let candidates = candidates
            .iter()
            .map(|candidate| {
                let support = candidate.settings.rate_control_support();
                EncoderCandidate {
                    id: candidate.capability.id.clone(),
                    codec: candidate.capability.codec.clone(),
                    hardware: candidate.capability.hardware,
                    limits: EncoderLimits {
                        max_width: candidate.settings.caps_width_max,
                        max_height: candidate.settings.caps_height_max,
                        cbr: support.cbr,
                        vbr: support.vbr,
                    },
                }
            })
            .collect::<Vec<_>>();
        plan_recording(&PlanInputs {
            quality: selections.quality,
            resolution: selections.resolution,
            fps_mode: selections.fps_mode,
            encoder: selections.encoder.clone(),
            native: Dimensions {
                width: display.width,
                height: display.height,
            },
            nominal_rate,
            candidates: &candidates,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn encode(
        display_id: &str,
        seconds: u32,
        output: &Path,
        nominal_rate: NominalRate,
        selections: &Selections,
        store: Option<&SettingsStore>,
        stall_consumer_ms: u32,
    ) -> Result<String, (RecorderError, Vec<PathBuf>)> {
        let displays = PlatformBackend
            .enumerate_displays()
            .map_err(|error| (error, Vec::new()))?;
        let Some(display) = displays
            .iter()
            .find(|display| display.id.0 == display_id)
            .cloned()
        else {
            return Err((
                RecorderError {
                    code: ErrorCode::InvalidRequest,
                    message: format!(
                        "The requested display {display_id} is not attached; enumerated displays:\n{}",
                        available_displays()
                    ),
                    recovery: RecoveryStatus::NoRecording,
                    action: "Copy a display identity from the enumerated list.".to_string(),
                },
                Vec::new(),
            ));
        };

        let plan =
            build_plan(&display, selections, nominal_rate).map_err(|error| (error, Vec::new()))?;
        let config = encoder_config(&plan).map_err(|error| (error, Vec::new()))?;
        let mut artifacts = ProofArtifacts::create(output).map_err(|error| (error, Vec::new()))?;
        let stream_path = artifacts.stream_path().to_path_buf();
        let sidecar_path = artifacts.sidecar_path().to_path_buf();
        let fail = |error: RecorderError| (error, vec![stream_path.clone(), sidecar_path.clone()]);

        // The frame-rate selection is a policy over the real captured timestamps: a manual
        // rate admits real frames on its grid and never invents one.
        let mut admitter = match plan.frame_rate {
            FrameRatePlan::SourcePaced => FrameAdmitter::auto(),
            FrameRatePlan::Fixed(rate) => FrameAdmitter::fixed(rate.num(), rate.den()),
        };

        let clock = std::sync::Arc::new(MasterClock::default());
        let cancellation = CaptureCancellation::default();
        let mut backend = PlatformBackend;
        let mut session = backend
            .start(
                &CaptureTarget::Display(DisplayId(display_id.to_string())),
                FpsMode::AutoMatchSource,
                CaptureContext {
                    clock,
                    cancellation: cancellation.clone(),
                },
            )
            .map_err(fail)?;

        // Wait for the first completed surface, bounded by the capture readiness path.
        let first = wait_for_frame(&mut session).map_err(fail)?;
        // The readiness frame is a real captured frame, so the policy sees it too: it
        // establishes the fixed grid's origin and is admitted with its own timestamp.
        if admitter.admit(first.timestamp).map_err(fail)? != FrameAdmission::Pass {
            return Err((
                RecorderError::invalid(
                    "The first captured frame was skipped by the frame-rate policy, which cannot establish the grid's origin.",
                ),
                Vec::new(),
            ));
        }
        let mut encoder = WindowsH264Encoder::open(config, &first.surface).map_err(fail)?;
        let settings = encoder.settings().clone();

        let started = Instant::now();
        let started_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or(0);
        let settings_record = SettingsRecord {
            kind: "settings",
            tool: "frame encode_probe",
            task: "FRAME-0104/FRAME-0207",
            display_id: display_id.to_string(),
            width: settings.width,
            height: settings.height,
            enumerated_source_nominal_hz: display.refresh_rate_hz,
            nominal_rate_num: settings.rate_num,
            nominal_rate_den: settings.rate_den,
            bitrate_bps: settings.bitrate_bps,
            max_bitrate_bps: settings.max_bitrate_bps,
            rate_control: settings.rate_control.describe(),
            preset: settings.preset.describe(),
            gop_frames: settings.gop_length,
            idr_period: settings.idr_period,
            frame_interval_p: settings.frame_interval_p,
            enable_ptd: settings.enable_ptd,
            enable_async: settings.enable_async,
            repeat_sps_pps: settings.repeat_spspps,
            profile_high: settings.profile_high,
            vui_full_range: settings.vui_full_range,
            caps_preset_available: settings.caps_preset_available,
            input_format: "DXGI_FORMAT_B8G8R8A8_UNORM registered as NV_ENC_BUFFER_FORMAT_ARGB",
            codec_config_bytes: settings.codec_config.len(),
            output_buffers: settings.output_buffers,
            output_buffer_capacity: settings.output_buffer_capacity,
            max_access_unit_bytes: settings.max_access_unit_bytes,
            stream_path: stream_path.display().to_string(),
            sidecar_path: sidecar_path.display().to_string(),
            started_unix_ms,
        };
        let frame_settings::ResolutionPlan::Native(native) = plan.resolution;
        let plan_record = PlanRecord {
            kind: "plan",
            encoder_id: plan.encoder.id.clone(),
            encoder_codec: plan.encoder.codec.clone(),
            encoder_hardware: plan.encoder.hardware,
            encoder_max_width: plan.encoder.limits.max_width,
            encoder_max_height: plan.encoder.limits.max_height,
            quality: match plan.quality {
                Quality::Standard => "standard",
                Quality::High => "high",
                Quality::Maximum => "maximum",
            },
            resolution: "native",
            source_width: native.width,
            source_height: native.height,
            frame_rate_mode: match plan.frame_rate {
                FrameRatePlan::SourcePaced => "sourcePaced",
                FrameRatePlan::Fixed(_) => "manualGrid",
            },
            nominal_rate_num: plan.nominal_rate.num().get(),
            nominal_rate_den: plan.nominal_rate.den().get(),
            calculated_bitrate_bps: plan.calculated_bitrate_bps,
            applied_bitrate_bps: plan.bitrate_bps,
            applied_peak_bitrate_bps: plan.peak_bitrate_bps,
            level: plan.level.name,
            level_ceiling_bps: plan.level_ceiling_bps,
            gop_frames: plan.gop_frames,
            rate_control: match plan.rate_control {
                RateControl::Cbr => "CBR",
                RateControl::Vbr => "VBR",
                RateControl::Cqp => "CQP",
            },
            preset: match plan.preset {
                EncoderPreset::Lossless => "lossless",
                EncoderPreset::LowLatency => "P1 / ultra-low-latency",
                EncoderPreset::HighQuality => "P5 / high-quality",
                EncoderPreset::MaximumQuality => "P7 / high-quality",
            },
            decisions: plan
                .decisions
                .iter()
                .map(|decision| PlanDecisionRecord {
                    code: format!("{:?}", decision.code),
                    detail: decision.detail.clone(),
                })
                .collect(),
        };
        artifacts
            .write_sidecar_line(&serde_json::to_string(&settings_record).map_err(|error| {
                (
                    serialization_error(error),
                    vec![stream_path.clone(), sidecar_path.clone()],
                )
            })?)
            .map_err(fail)?;
        artifacts
            .write_sidecar_line(&serde_json::to_string(&plan_record).map_err(|error| {
                (
                    serialization_error(error),
                    vec![stream_path.clone(), sidecar_path.clone()],
                )
            })?)
            .map_err(fail)?;

        let (witness_stop, witness_rx) = std::sync::mpsc::channel();
        let witness_thread = std::thread::spawn(move || {
            while witness_rx
                .recv_timeout(Duration::from_secs(1))
                .is_err_and(|error| matches!(error, std::sync::mpsc::RecvTimeoutError::Timeout))
            {
                eprintln!(
                    "native witness: {}",
                    WindowsH264Encoder::native_call_witness()
                );
            }
        });
        struct WitnessSampler(
            std::sync::mpsc::Sender<()>,
            Option<std::thread::JoinHandle<()>>,
        );
        impl Drop for WitnessSampler {
            fn drop(&mut self) {
                let _ = self.0.send(());
                if let Some(thread) = self.1.take() {
                    let _ = thread.join();
                }
            }
        }
        let _witness = WitnessSampler(witness_stop, Some(witness_thread));
        let deadline = started + Duration::from_secs(u64::from(seconds));
        let mut written = Written {
            packets: 0,
            bytes: 0,
            monotonic: true,
            first_pts: None,
            last_pts: None,
            first_keyframe: false,
            initialization_written: false,
        };
        // The caller holds at most one not-yet-submitted frame (the lookahead that gives
        // the previous frame its exact duration) and at most one frame waiting for a
        // bounded output credit. Both stay owned here; the capture pool's four slots are
        // the ultimate bound, and pressure pauses acquisition instead of dropping frames.
        let mut lookahead: Option<CapturedFrame<WindowsSurface>> = Some(first);
        let mut pending: Option<EncoderInput<CapturedFrame<WindowsSurface>>> = None;
        let mut drained_early = false;
        // Optional diagnostic pressure phase: the delivery queue is deliberately not
        // consumed for this long, so the bounded packet credits fill and `submit` must
        // return the owned input instead of growing memory.
        let pressure_deadline = started + Duration::from_millis(u64::from(stall_consumer_ms));
        let mut pressure_backpressure = 0u64;
        let mut pressure_held_ms = 0u128;
        let mut pressure_peak_slots = 0u64;
        let mut pressure_peak_bytes = 0u64;
        let mut pressure_peak_held = 0u64;
        // Native progress is tracked independently of the run deadline, because a driver
        // call that stops completing work cannot be forcibly cancelled: without this the
        // proof would wait forever instead of reporting the stall it observed.
        let mut last_progress = Instant::now();
        let mut last_trace = Instant::now();
        let mut last_progress_count = 0u64;
        let mut stalled = false;

        let outcome: Result<(), RecorderError> = (|| {
            while !drained_early {
                let in_pressure = stall_consumer_ms > 0 && Instant::now() < pressure_deadline;
                if !in_pressure {
                    drained_early = drain_packets(&mut encoder, &mut artifacts, &mut written)?;
                    if drained_early {
                        break;
                    }
                }
                if let Some(input) = pending.take() {
                    match encoder.submit(input)? {
                        SubmitOutcome::Accepted => {}
                        SubmitOutcome::Backpressure(input) => {
                            if in_pressure {
                                pressure_backpressure += 1;
                            }
                            pending = Some(input);
                        }
                    }
                }
                let counters = encoder.counters();
                // Bounded watchdog trace: if a parent process has to terminate this child,
                // the trace names the exact native call it was inside.
                if last_trace.elapsed() >= Duration::from_secs(2) {
                    last_trace = Instant::now();
                    eprintln!(
                        "encode_probe trace: elapsed_ms={} written={} {}",
                        started.elapsed().as_millis(),
                        written.packets,
                        encoder.live_trace()
                    );
                }
                let progress = counters
                    .packets_emitted
                    .saturating_add(counters.native_submissions)
                    .saturating_add(counters.native_buffered)
                    .saturating_add(counters.native_failures);
                if progress != last_progress_count {
                    last_progress_count = progress;
                    last_progress = Instant::now();
                }
                // A pending input or an in-flight submission means work is expected; a
                // five-second gap with the native watchdog unable to fire (it is checked
                // between calls) is reported instead of waited out.
                let work_expected = pending.is_some() || counters.submitted_in_flight() > 0;
                if work_expected && last_progress.elapsed() > PROGRESS_WATCHDOG {
                    stalled = true;
                    return Err(RecorderError {
                        code: ErrorCode::Internal,
                        message: format!(
                            "The hardware encoder made no progress for {} ms: {} submission(s) in flight, {} lease(s) retained, {} packet(s) delivered, {} emitted and {} accepted. The last native call was: {}. A driver call inside the encoder worker cannot be forcibly cancelled, so the run is reported as stalled instead of waiting forever.",
                            last_progress.elapsed().as_millis(),
                            counters.submitted_in_flight(),
                            counters.retained_leases,
                            written.packets,
                            counters.packets_emitted,
                            counters.accepted_inputs,
                            encoder.native_call_phase()
                        ),
                        recovery: RecoveryStatus::Unknown,
                        action: "The stream is incomplete: discard the artifact and report the stall; the process terminates without completing native teardown."
                            .to_string(),
                    });
                }
                if in_pressure {
                    // Peaks are sampled every iteration, whether or not the caller is
                    // holding an input, so a credits-only pressure phase is still measured.
                    pressure_peak_slots = pressure_peak_slots.max(counters.retained_leases);
                    pressure_peak_bytes = pressure_peak_bytes.max(counters.bytes_held);
                    pressure_peak_held = pressure_peak_held.max(counters.packets_held);
                    if pending.is_some() {
                        pressure_held_ms = started.elapsed().as_millis();
                    }
                }
                if pending.is_some() {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                if !in_pressure && Instant::now() >= deadline {
                    break;
                }
                match session.try_frame()? {
                    Some(frame) => {
                        // The frame-rate selection decides admission over the frame's own
                        // timestamp: a skipped frame is a policy decision, the admitted
                        // timestamp is never moved and no frame is ever duplicated.
                        match admitter.admit(frame.timestamp)? {
                            FrameAdmission::Pass => {
                                if let Some(previous) = lookahead.take() {
                                    let duration =
                                        duration_until(previous.timestamp, frame.timestamp)?;
                                    pending = Some(EncoderInput {
                                        input: previous,
                                        duration_ns: duration,
                                    });
                                }
                                lookahead = Some(frame);
                            }
                            FrameAdmission::SkipByPolicy => drop(frame),
                        }
                    }
                    None => std::thread::sleep(Duration::from_millis(1)),
                }
            }

            // The final frame has no strictly later source timestamp, so it receives the
            // explicit nominal period rounded up to nanoseconds, recorded as an estimate.
            if let Some(input) = pending.take() {
                submit_until_accepted(&mut encoder, &mut artifacts, &mut written, input)?;
            }
            if let Some(last) = lookahead.take() {
                submit_until_accepted(
                    &mut encoder,
                    &mut artifacts,
                    &mut written,
                    EncoderInput {
                        input: last,
                        duration_ns: nominal_duration(
                            plan.nominal_rate.num(),
                            plan.nominal_rate.den(),
                        ),
                    },
                )?;
            }

            // The encoder drains and stops before the capture session, because the
            // measured native behaviour is that stopping capture while an NVENC session
            // is still encoding on the same D3D11 device can leave the capture worker
            // inside its copy/query path for an unbounded time (see the FRAME-0104
            // evidence). The other order is still covered by the hardware test
            // `held_leases_survive_capture_stop_until_native_completion`, which proves
            // the leases stay valid across a capture stop.
            encoder.begin_drain()?;
            loop {
                if drain_packets(&mut encoder, &mut artifacts, &mut written)? {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            encoder.stop()?;
            session.stop()?;
            Ok(())
        })();

        let counters = encoder.counters();
        let identities = counters.identities();
        if let Err(error) = outcome {
            if stalled {
                // A driver call inside the encoder worker has stopped making progress and
                // cannot be forcibly cancelled, so joining it would block this proof
                // forever. The stall is reported with the exact counters, the capture
                // session is stopped, and the process terminates without detaching the
                // worker or recycling its resources: an explicit, bounded failure.
                eprintln!("encode_probe stalled: {:?}", error.code);
                eprintln!("{}", error.message);
                eprintln!("action: {}", error.action);
                eprintln!(
                    "stall detail: submitted_in_flight={} retained_leases={} packets_written={} packets_emitted={} bytes_written={}",
                    counters.submitted_in_flight(),
                    counters.retained_leases,
                    written.packets,
                    counters.packets_emitted,
                    written.bytes
                );
                eprintln!(
                    "stream artifact (incomplete, not evidence): {}",
                    stream_path.display()
                );
                eprintln!("sidecar artifact (incomplete): {}", sidecar_path.display());
                // Neither stop is attempted here: with a native call that has not returned,
                // a join cannot complete, so the process terminates without detaching or
                // recycling anything. That is a diagnostic bound, not a completed cleanup,
                // and it never satisfies acceptance.
                std::process::exit(1);
            }
            let _ = encoder.stop();
            let _ = session.stop();
            return Err((error, vec![stream_path, sidecar_path]));
        }
        if let Err(detail) = &identities {
            let _ = encoder.stop();
            let _ = session.stop();
            return Err((
                RecorderError {
                    code: ErrorCode::Internal,
                    message: format!(
                        "The encoder counters do not reconcile after a completed drain: {detail}"
                    ),
                    recovery: RecoveryStatus::Unknown,
                    action: "Do not treat this stream as evidence.".to_string(),
                },
                vec![stream_path, sidecar_path],
            ));
        }
        // Both conditions are required for a pressure experiment to pass: the bounded
        // delivery credits must actually be reached *and* a real owned input must be
        // refused and returned by Backpressure. Either one missing is inconclusive.
        let pressure_reached_credits = pressure_peak_held >= frame_windows::MAX_PACKETS as u64;
        if stall_consumer_ms > 0 && (pressure_backpressure == 0 || !pressure_reached_credits) {
            let _ = session.stop();
            return Err((
                RecorderError {
                    code: ErrorCode::Internal,
                    message: format!(
                        "The pressure experiment is inconclusive: {} ms of consumer stall produced credits_reached={} and backpressure_returns={}, so the bounded-credit pressure path was not fully exercised (peak retained leases {}, peak held packets {}, peak held bytes {}).",
                        stall_consumer_ms,
                pressure_reached_credits,
                pressure_backpressure,
                pressure_peak_slots,
                pressure_peak_held,
                pressure_peak_bytes
                    ),
                    recovery: RecoveryStatus::Unknown,
                    action: "Re-run with a longer --stall-consumer-ms; a run without observed pressure is not evidence."
                        .to_string(),
                },
                vec![stream_path, sidecar_path],
            ));
        }
        if written.packets == 0 {
            let _ = session.stop();
            return Err((
                RecorderError {
                    code: ErrorCode::Internal,
                    message: "The encode produced no access unit, so there is no proof to record."
                        .to_string(),
                    recovery: RecoveryStatus::Unknown,
                    action: "Keep capture source-paced and retry; an empty stream is not evidence."
                        .to_string(),
                },
                vec![stream_path, sidecar_path],
            ));
        }

        let elapsed = started.elapsed().as_secs_f64();
        let admission = admitter.counters();
        let summary = SummaryRecord {
            kind: "summary",
            accepted_inputs: counters.accepted_inputs,
            backpressure_returns: counters.backpressure_returns,
            native_submissions: counters.native_submissions,
            native_buffered: counters.native_buffered,
            native_failures: counters.native_failures,
            packets_emitted: counters.packets_emitted,
            packets_dequeued: counters.packets_dequeued,
            packets_written: written.packets,
            bytes_written: written.bytes,
            packets_discarded_on_stop: counters.packets_discarded_on_stop,
            retained_leases: counters.retained_leases,
            registrations: counters.registrations,
            unregistrations: counters.unregistrations,
            eos_submissions: counters.eos_submissions,
            eos_acknowledgements: counters.eos_acknowledgements,
            native_busy_retries: counters.native_busy_retries,
            timestamp_mismatches: counters.timestamp_mismatches,
            timestamp_regressions: counters.timestamp_regressions,
            drained: encoder.drained(),
            monotonic_timestamps: written.monotonic,
            first_pts_ns: written.first_pts,
            last_pts_ns: written.last_pts,
            first_keyframe: written.first_keyframe,
            initialization_bytes_written: written.initialization_written,
            duration_s: elapsed,
            identities_reconciled: identities.is_ok(),
            frames_seen: admission.frames_seen,
            frames_passed: admission.frames_passed,
            frames_skipped_by_policy: admission.frames_skipped_by_policy,
        };
        // The selections this run applied are persisted through the versioned document,
        // after a completed stream: the file records what really ran, not an intention.
        let output_directory = output
            .parent()
            .map(|parent| parent.display().to_string())
            .unwrap_or_default();
        let output_stem = output
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "stream".to_string());
        let persisted = store.map(|store| {
            store.save(&RecordingRequest {
                target: CaptureTarget::Display(DisplayId(display_id.to_string())),
                fps_mode: selections.fps_mode,
                resolution: selections.resolution,
                quality: selections.quality,
                encoder: selections.encoder.clone(),
                optimization: None,
                // This bounded proof captures no audio, so it stores no audio route.
                audio_routes: Vec::new(),
                camera: None,
                show_cursor: true,
                output: OutputSettings {
                    directory: output_directory,
                    file_stem: output_stem,
                },
            })
        });
        if let Some(Err(error)) = &persisted {
            let _ = session.stop();
            return Err((
                RecorderError {
                    code: ErrorCode::Internal,
                    message: format!(
                        "The stream completed, but the recording settings could not be persisted: {error}"
                    ),
                    recovery: RecoveryStatus::Unknown,
                    action: "The stream and sidecar are complete; the settings document is not."
                        .to_string(),
                },
                vec![stream_path, sidecar_path],
            ));
        }
        let persistence_record = SettingsPersistenceRecord {
            kind: "settingsPersistence",
            settings_path: store.map(|store| store.path().display().to_string()),
            loaded_from_disk: selections.loaded_from_disk,
            saved: persisted.is_some_and(|result| result.is_ok()),
        };
        artifacts
            .write_sidecar_line(
                &serde_json::to_string(&persistence_record).map_err(|error| {
                    (
                        serialization_error(error),
                        vec![stream_path.clone(), sidecar_path.clone()],
                    )
                })?,
            )
            .map_err(|error| (error, vec![stream_path.clone(), sidecar_path.clone()]))?;
        let sidecar_lines = match finish_artifacts(artifacts, &summary) {
            Ok(lines) => lines,
            Err(error) => {
                let _ = session.stop();
                return Err((error, vec![stream_path, sidecar_path]));
            }
        };

        let mut report = String::new();
        report.push_str(
            "Frame Windows hardware H.264 encode proof (FRAME-0104, quality policy FRAME-0207)\n",
        );
        report.push_str(
            "This proof writes one raw Annex-B elementary stream and one diagnostic JSONL sidecar: no container, no mux, no audio, no recording and no UI binding.\n",
        );
        report.push_str(&format!(
            "requested: display_id={display_id} seconds={seconds} output={} nominal_rate={}/{}\n",
            stream_path.display(),
            plan.nominal_rate.num().get(),
            plan.nominal_rate.den().get()
        ));
        report.push_str(&format!(
            "source: enumerated_nominal_hz={:?} geometry={}x{}\n",
            display.refresh_rate_hz, display.width, display.height
        ));
        report.push_str(&format!(
            "plan: quality={:?} frame_rate={:?} resolution=native {}x{} encoder={} candidates_selected_by_policy=true\n",
            plan.quality,
            plan.frame_rate,
            native.width,
            native.height,
            plan.encoder.id
        ));
        report.push_str(&format!(
            "plan_bitrate: calculated={} applied={} peak={} level={} level_ceiling={}\n",
            plan.calculated_bitrate_bps,
            plan.bitrate_bps,
            plan.peak_bitrate_bps,
            plan.level.name,
            plan.level_ceiling_bps
        ));
        for decision in &plan.decisions {
            report.push_str(&format!(
                "plan_decision: {:?}: {}\n",
                decision.code, decision.detail
            ));
        }
        report.push_str(&format!(
            "encoder: {}x{} at {}/{} nominal, {} {} bit/s (peak {}), GOP {} (IDR {}), frameIntervalP={}, ptd={}, async={}, repeatSPSPPS={}, profile_high={}, vui_full_range={}, preset={}, {} initialization bytes, {} output buffers of {} bytes\n",
            settings.width,
            settings.height,
            settings.rate_num,
            settings.rate_den,
            settings.rate_control.describe(),
            settings.bitrate_bps,
            settings.max_bitrate_bps,
            settings.gop_length,
            settings.idr_period,
            settings.frame_interval_p,
            settings.enable_ptd,
            settings.enable_async,
            settings.repeat_spspps,
            settings.profile_high,
            settings.vui_full_range,
            settings.preset.describe(),
            settings.codec_config.len(),
            settings.output_buffers,
            settings.output_buffer_capacity
        ));
        report.push_str(&format!(
            "admission: frames_seen={} frames_passed={} frames_skipped_by_policy={} (policy skips, not capture loss)\n",
            admission.frames_seen, admission.frames_passed, admission.frames_skipped_by_policy
        ));
        report.push_str(&format!(
            "artifacts: stream={} bytes={} packets={} monotonic={} first_keyframe={} first_pts_ns={:?} last_pts_ns={:?}\n",
            stream_path.display(),
            written.bytes,
            written.packets,
            written.monotonic,
            written.first_keyframe,
            written.first_pts,
            written.last_pts
        ));
        report.push_str(&format!(
            "sidecar: {} ({sidecar_lines} lines)\n",
            sidecar_path.display()
        ));
        report.push_str(&format!(
            "delivery: accepted={} emitted={} written={} backpressure_returns={} native_submissions={} buffered={} busy_retries={}\n",
            counters.accepted_inputs,
            counters.packets_emitted,
            written.packets,
            counters.backpressure_returns,
            counters.native_submissions,
            counters.native_buffered,
            counters.native_busy_retries
        ));
        report.push_str(&format!(
            "drain: drained={} eos_submissions={} eos_acknowledgements={}\n",
            encoder.drained(),
            counters.eos_submissions,
            counters.eos_acknowledgements
        ));
        report.push_str(&format!(
            "pressure: requested_hold_ms={stall_consumer_ms} observed_hold_ms={pressure_held_ms} backpressure_returns={pressure_backpressure} peak_retained_leases={pressure_peak_slots} peak_held_packets={pressure_peak_held} peak_held_bytes={pressure_peak_bytes} credits_reached={pressure_reached_credits}\n"
        ));
        report.push_str(&format!(
            "ownership: retained_leases={} registrations={} unregistrations={} packets_held={}\n",
            counters.retained_leases,
            counters.registrations,
            counters.unregistrations,
            counters.packets_held
        ));
        report.push_str(&format!(
            "accounting: every checked identity holds ({})\n",
            identities.is_ok()
        ));
        report.push_str(&format!(
            "settings: path={:?} loaded_from_disk={} saved={}\n",
            persistence_record.settings_path,
            persistence_record.loaded_from_disk,
            persistence_record.saved
        ));
        report.push_str(&format!(
            "note: the enumerated nominal rate, the requested rational rate and the achieved access-unit rate are separate observations; only {:.2} written access units per second were measured, and a raw Annex-B stream carries no container timestamps. The plan's bitrate and rate are configuration values, not measurements.\n",
            written.packets as f64 / elapsed.max(f64::EPSILON)
        ));
        report.push_str("capture diagnostics:\n");
        match session.diagnostics() {
            Ok(diagnostics) => {
                for line in diagnostics.lines().take(40) {
                    report.push_str("  ");
                    report.push_str(line);
                    report.push('\n');
                }
            }
            Err(error) => report.push_str(&format!("  (unavailable: {})\n", error.message)),
        }
        Ok(report)
    }

    /// Writes the final summary line and closes both artifacts.
    fn finish_artifacts(
        mut artifacts: ProofArtifacts,
        summary: &SummaryRecord,
    ) -> Result<u64, RecorderError> {
        artifacts
            .write_sidecar_line(&serde_json::to_string(summary).map_err(serialization_error)?)?;
        let lines = artifacts.sidecar_lines();
        artifacts.finish()?;
        Ok(lines)
    }

    /// Retries one owned input until the encoder accepts it.
    ///
    /// A stalled native encoder surfaces as an error from `try_packet` instead of an
    /// unbounded retry loop, and a terminal failure is never swallowed.
    fn submit_until_accepted<I>(
        encoder: &mut impl PacketEncoder<I>,
        artifacts: &mut ProofArtifacts,
        written: &mut Written,
        input: I,
    ) -> Result<(), RecorderError> {
        let mut pending = Some(input);
        let deadline = Instant::now() + PROGRESS_WATCHDOG;
        loop {
            if let Some(current) = pending.take() {
                match encoder.submit(current)? {
                    SubmitOutcome::Accepted => return Ok(()),
                    SubmitOutcome::Backpressure(current) => pending = Some(current),
                }
            }
            // Every packet the encoder hands out goes through the same artifact writer,
            // sidecar and byte accounting, including final-tail admission and drain.
            if drain_packets(encoder, artifacts, written)? {
                return Err(RecorderError::invalid(
                    "The encoder drained before accepting its final input.",
                ));
            }
            if Instant::now() >= deadline {
                return Err(RecorderError {
                    code: ErrorCode::Internal,
                    message: format!(
                        "The hardware encoder did not accept a final input within {} ms.",
                        PROGRESS_WATCHDOG.as_millis()
                    ),
                    recovery: RecoveryStatus::Unknown,
                    action: "The stream is incomplete: discard the artifact and retry the proof."
                        .to_string(),
                });
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Waits for the first completed capture surface with a bounded timeout.
    fn wait_for_frame(
        session: &mut WindowsCaptureSession,
    ) -> Result<CapturedFrame<WindowsSurface>, RecorderError> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(frame) = session.try_frame()? {
                return Ok(frame);
            }
            if Instant::now() >= deadline {
                return Err(RecorderError {
                    code: ErrorCode::SourceLost,
                    message: "The capture session delivered no completed surface within 5 s."
                        .to_string(),
                    recovery: RecoveryStatus::NoRecording,
                    action: "Verify the display is active and try again.".to_string(),
                });
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        use frame_clock::MediaTimestamp;
        use frame_encode::{BitstreamFormat, EncodedPacket, EncodedStreamInfo, EncoderCapability};
        use std::collections::VecDeque;
        struct TailEncoder {
            info: EncodedStreamInfo,
            packets: VecDeque<EncodedPacket>,
            refused: bool,
            accepted: Option<Box<u64>>,
        }
        fn packet(pts: u64, data: Vec<u8>) -> EncodedPacket {
            EncodedPacket {
                track_id: 0,
                pts: MediaTimestamp(pts),
                dts: MediaTimestamp(pts),
                duration_ns: 1,
                keyframe: pts == 1,
                data,
            }
        }
        impl PacketEncoder<Box<u64>> for TailEncoder {
            fn submit(
                &mut self,
                input: Box<u64>,
            ) -> Result<SubmitOutcome<Box<u64>>, RecorderError> {
                if !self.refused {
                    self.refused = true;
                    return Ok(SubmitOutcome::Backpressure(input));
                }
                assert!(
                    self.packets.is_empty(),
                    "tail admission must consume the available packet"
                );
                self.packets.push_back(packet(*input, vec![4, 5]));
                self.accepted = Some(input);
                Ok(SubmitOutcome::Accepted)
            }
            fn try_packet(&mut self) -> Result<EncoderPoll, RecorderError> {
                Ok(self
                    .packets
                    .pop_front()
                    .map_or(EncoderPoll::Pending, EncoderPoll::Packet))
            }
            fn stream_info(&self) -> &EncodedStreamInfo {
                &self.info
            }
            fn begin_drain(&mut self) -> Result<(), RecorderError> {
                Ok(())
            }
            fn stop(&mut self) -> Result<(), RecorderError> {
                Ok(())
            }
        }
        #[test]
        fn final_tail_retry_writes_real_stream_and_contiguous_jsonl_offsets() {
            let nz = NonZeroU32::new(1).unwrap();
            let mut encoder = TailEncoder {
                info: EncodedStreamInfo {
                    capability: EncoderCapability {
                        id: "scripted-writer-test".into(),
                        codec: "h264".into(),
                        hardware: false,
                    },
                    width: nz,
                    height: nz,
                    nominal_rate_num: nz,
                    nominal_rate_den: nz,
                    bitstream_format: BitstreamFormat::H264AnnexB,
                    colour: None,
                    codec_config: vec![9, 8],
                    allows_reordering: false,
                },
                packets: VecDeque::from([packet(1, vec![1, 2, 3])]),
                refused: false,
                accepted: None,
            };
            let path = std::env::temp_dir().join(format!(
                "frame-tail-writer-{}-{}.h264",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let mut artifacts = ProofArtifacts::create(&path).unwrap();
            let mut written = Written {
                packets: 0,
                bytes: 0,
                monotonic: true,
                first_pts: None,
                last_pts: None,
                first_keyframe: false,
                initialization_written: false,
            };
            let tail = Box::new(2);
            let identity = &*tail as *const u64;
            submit_until_accepted(&mut encoder, &mut artifacts, &mut written, tail).unwrap();
            assert_eq!(
                &**encoder.accepted.as_ref().unwrap() as *const u64,
                identity
            );
            drain_packets(&mut encoder, &mut artifacts, &mut written).unwrap();
            artifacts.finish().unwrap();
            assert!(encoder.refused && written.monotonic);
            assert_eq!(written.packets, 2);
            assert_eq!(std::fs::read(&path).unwrap(), [9, 8, 1, 2, 3, 4, 5]);
            let jsonl = std::fs::read_to_string(format!("{}.jsonl", path.display())).unwrap();
            let rows = jsonl
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0]["offset"], 2);
            assert_eq!(rows[0]["bytes"], 3);
            assert_eq!(rows[1]["offset"], 5);
            assert_eq!(rows[1]["bytes"], 2);
            assert_eq!(rows[1]["pts_ns"], 2);
        }
    }
}
