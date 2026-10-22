// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! FRAME-0205 GPU region capture probe.
//!
//! It records one rectangle of one selected display at its real resolution and writes one
//! single-video Matroska file, so an independent decoder can check that the crop really is
//! the requested pixels instead of a claim.
//!
//! ```text
//! cargo run --locked -p frame-windows --example region_capture_probe -- \
//!     --display-id ID --pattern --seconds 2 --output out/region.mkv \
//!     --rate-num 180 --rate-den 1
//! ```
//!
//! Two modes, exactly one of them always explicit:
//!
//! - `--pattern` shows a known-content window over the whole selected display — vertical
//!   80 px colour bars above a dark band that starts at 60 % of the display height — and
//!   records a fixed rectangle that starts three bars in and crosses the band edge. The
//!   report names the palette, the bar width and the band edge in *display-local* physical
//!   pixels, so `verify_region_capture.py` can derive the expected colour of every decoded
//!   pixel and check the exact crop offset as well as the crop size.
//! - `--region X,Y,W,H` records a caller-supplied canonical region (the display's own
//!   physical pixel space, top-left origin, half-open rectangle) and reports it unchanged.
//!
//! What this probe is not: a recorder, an audio path, a UI binding or a container
//! implementation. It copies on the GPU, encodes with the accepted hardware encoder and
//! writes through the accepted container writer. The output path is created exclusively, so
//! an existing recording is never overwritten.

#![cfg_attr(target_os = "windows", allow(unsafe_code))]

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
    eprintln!("GPU region capture is implemented for Windows only.");
    std::process::ExitCode::FAILURE
}

#[cfg(target_os = "windows")]
mod probe {
    use std::num::NonZeroU32;
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use frame_capture::{
        CaptureCancellation, CaptureContext, CaptureSession, CapturedFrame, DisplayBackend,
    };
    use frame_clock::MasterClock;
    use frame_contracts::{DisplayInfo, ErrorCode, RecorderError, RecoveryStatus, Region};
    use frame_encode::{
        EncoderInput, EncoderPoll, PacketEncoder, SubmitOutcome, duration_until, nominal_duration,
    };
    use frame_mux::{ContainerWriter, MkvVideoWriter};
    use frame_windows::{
        NativeEncoderConfig, PlatformBackend, RegionCaptureSession, WindowsH264Encoder,
        WindowsSurface,
    };

    use crate::pattern::PatternSpec;

    const USAGE: &str = "usage: region_capture_probe --output PATH.mkv --rate-num N --rate-den N \
        [--pattern | --region X,Y,W,H] [--display-id ID] [--seconds 1..60] [--settle-ms N]";

    /// Longest accepted probe duration.
    const MAX_SECONDS: u32 = 60;
    /// Bounded wall-clock budget for the first completed crop surface.
    const PREPARATION_WATCHDOG: Duration = Duration::from_secs(5);
    /// Bounded budget between two observable steps while the recording owns work.
    const PROGRESS_WATCHDOG: Duration = Duration::from_secs(5);
    /// The compositor is given this bounded settle time after the pattern window painted, so
    /// the first recorded surface is the known content rather than the desktop behind it.
    const DEFAULT_SETTLE_MS: u32 = 500;
    /// The pattern's bar width, in the display's physical pixels.
    const STRIPE_WIDTH: u32 = 80;
    /// The recorded rectangle in `--pattern` mode: it starts three bars in, so a wrong
    /// horizontal offset shows a different colour, and crosses the band edge exactly
    /// [`PATTERN_BARS_ROWS`] rows below its top, so a wrong vertical offset shows the band.
    const PATTERN_REGION_X: i32 = 240;
    const PATTERN_REGION_WIDTH: u32 = 640;
    const PATTERN_REGION_HEIGHT: u32 = 360;
    const PATTERN_BARS_ROWS: u32 = 60;
    /// The known-content palette (RGB): six saturated colours with unambiguous dominant
    /// channels, so a decoded pixel can be classified without an image framework.
    const PALETTE: [[u8; 3]; 6] = [
        [255, 0, 0],
        [0, 255, 0],
        [0, 0, 255],
        [255, 255, 0],
        [0, 255, 255],
        [255, 0, 255],
    ];
    /// The reference colour of the lower part of the pattern window (RGB).
    const BAND_COLOR: [u8; 3] = [16, 16, 16];

    struct Options {
        display_id: Option<String>,
        region: Option<(i32, i32, u32, u32)>,
        pattern: bool,
        seconds: u32,
        settle_ms: u32,
        output: Option<PathBuf>,
        rate_num: Option<u32>,
        rate_den: Option<u32>,
    }

    fn parse_options() -> Result<Options, String> {
        let arguments: Vec<String> = std::env::args().skip(1).collect();
        let mut options = Options {
            display_id: None,
            region: None,
            pattern: false,
            seconds: 2,
            settle_ms: DEFAULT_SETTLE_MS,
            output: None,
            rate_num: None,
            rate_den: None,
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
                "--region" => {
                    let raw = take("--region")?;
                    let parts: Vec<&str> = raw.split(',').collect();
                    if parts.len() != 4 {
                        return Err(format!("--region expects X,Y,W,H, not {raw}"));
                    }
                    let number = |value: &str| -> Result<i64, String> {
                        value
                            .trim()
                            .parse()
                            .map_err(|_| format!("--region component {value} is not a number"))
                    };
                    let (x, y, width, height) = (
                        number(parts[0])?,
                        number(parts[1])?,
                        number(parts[2])?,
                        number(parts[3])?,
                    );
                    let (Ok(x), Ok(y), Ok(width), Ok(height)) = (
                        i32::try_from(x),
                        i32::try_from(y),
                        u32::try_from(width),
                        u32::try_from(height),
                    ) else {
                        return Err(format!("--region {raw} is outside the accepted range"));
                    };
                    options.region = Some((x, y, width, height));
                }
                "--pattern" => options.pattern = true,
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
                "--settle-ms" => {
                    let raw = take("--settle-ms")?;
                    options.settle_ms = raw
                        .parse()
                        .map_err(|_| format!("--settle-ms expects a number, not {raw}"))?;
                    if options.settle_ms > 5_000 {
                        return Err("--settle-ms must be at most 5000".to_string());
                    }
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
        if options.pattern == options.region.is_some() {
            return Err(format!(
                "choose exactly one mode: --pattern or --region X,Y,W,H\n{USAGE}"
            ));
        }
        if options.region.is_some() && options.display_id.is_none() {
            return Err(
                "--region needs --display-id: a canonical region names exactly one display"
                    .to_string(),
            );
        }
        Ok(options)
    }

    /// The bounded counts one recording delivered.
    #[derive(Clone, Copy, Debug, Default)]
    struct Stats {
        capture_frames: u64,
        accepted_frames: u64,
        backpressure_returns: u64,
        packets_written: u64,
        packet_bytes: u64,
        keyframes: u64,
        first_pts_ns: Option<u64>,
        last_pts_ns: Option<u64>,
        first_container_pts_ns: Option<u64>,
        last_container_end_ns: u64,
        prepared_ms: u128,
        drained_ms: u128,
    }

    /// Everything one successful run reports.
    struct Completed {
        artifact: PathBuf,
        duration_ms: f64,
        stats: Stats,
        crop_diagnostics: String,
        encoder_diagnostics: String,
        display_id: String,
        display_width: u32,
        display_height: u32,
        display_scale_factor: f64,
        region: Region,
    }

    pub(super) fn run() -> ExitCode {
        let options = match parse_options() {
            Ok(options) => options,
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::FAILURE;
            }
        };
        match record(&options) {
            Ok(done) => {
                let pattern = if options.pattern {
                    serde_json::json!({
                        "stripeWidth": STRIPE_WIDTH,
                        "palette": PALETTE,
                        "bandColor": BAND_COLOR,
                        "barsRows": PATTERN_BARS_ROWS,
                        "space": "display-local physical pixels of displayId",
                    })
                } else {
                    serde_json::Value::Null
                };
                let value = serde_json::json!({
                    "kind": "frame-0205-region-probe",
                    "mode": if options.pattern { "pattern" } else { "explicit" },
                    "artifact": done.artifact.to_string_lossy(),
                    "artifactDurationMs": done.duration_ms,
                    "seconds": options.seconds,
                    "rateNum": options.rate_num,
                    "rateDen": options.rate_den,
                    "displayId": done.display_id,
                    "displayWidth": done.display_width,
                    "displayHeight": done.display_height,
                    "displayScaleFactor": done.display_scale_factor,
                    "region": {
                        "x": done.region.x,
                        "y": done.region.y,
                        "width": done.region.width,
                        "height": done.region.height,
                    },
                    "pattern": pattern,
                    "stats": {
                        "captureFrames": done.stats.capture_frames,
                        "acceptedFrames": done.stats.accepted_frames,
                        "backpressureReturns": done.stats.backpressure_returns,
                        "packetsWritten": done.stats.packets_written,
                        "packetBytes": done.stats.packet_bytes,
                        "keyframes": done.stats.keyframes,
                        "firstPtsNs": done.stats.first_pts_ns,
                        "lastPtsNs": done.stats.last_pts_ns,
                        "firstContainerPtsNs": done.stats.first_container_pts_ns,
                        "lastContainerEndNs": done.stats.last_container_end_ns,
                        "preparedMs": done.stats.prepared_ms,
                        "drainedMs": done.stats.drained_ms,
                    },
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&value).unwrap_or_default()
                );
                // Diagnostics stay on stderr: stdout carries the machine-readable report the
                // decode-side verifier reads.
                eprintln!();
                eprintln!("{}", done.crop_diagnostics);
                eprintln!("{}", done.encoder_diagnostics);
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("region_capture_probe failed: {:?}", error.code);
                eprintln!("{}", error.message);
                eprintln!("action: {}", error.action);
                ExitCode::FAILURE
            }
        }
    }

    fn record(options: &Options) -> Result<Completed, RecorderError> {
        let started = Instant::now();
        let backend = PlatformBackend;
        let displays = backend.enumerate_displays()?;
        let display = select_display(&displays, options)?;
        let pattern = options.pattern.then(|| PatternSpec {
            stripe_width: STRIPE_WIDTH,
            palette: PALETTE,
            band_color: BAND_COLOR,
            band_height: band_height(display),
        });
        let region = match &options.region {
            Some((x, y, width, height)) => Region {
                display_id: display.id.clone(),
                x: *x,
                y: *y,
                width: *width,
                height: *height,
            },
            None => pattern_region(display)?,
        };
        // The canonical region is validated before any native session exists, so an
        // out-of-bounds, empty or non-encodable rectangle fails without starting capture.
        let bounds = frame_settings::validate_region(&region, display)?;
        let rate_num = NonZeroU32::new(options.rate_num.unwrap_or(0))
            .ok_or_else(|| invalid_error("--rate-num must be a positive number.".to_string()))?;
        let rate_den = NonZeroU32::new(options.rate_den.unwrap_or(0))
            .ok_or_else(|| invalid_error("--rate-den must be a positive number.".to_string()))?;
        let config =
            NativeEncoderConfig::proof(bounds.width(), bounds.height(), rate_num, rate_den)?;
        let output = options
            .output
            .clone()
            .ok_or_else(|| invalid_error("--output is required.".to_string()))?;
        if output.exists() {
            return Err(RecorderError {
                code: ErrorCode::InvalidOutputPath,
                message: format!(
                    "The output path {} already exists; this probe never overwrites a recording.",
                    output.display()
                ),
                recovery: RecoveryStatus::NoRecording,
                action: "Choose a new output path inside an existing writable directory."
                    .to_string(),
            });
        }

        // The known-content window is created before capture starts and lives until the file
        // is written, so every recorded surface shows the same pattern.
        let fixture = match &pattern {
            Some(spec) => Some(crate::pattern::PatternWindow::show(
                spec.clone(),
                display.x,
                display.y,
                display.width,
                display.height,
            )?),
            None => None,
        };
        if let Some(fixture) = &fixture {
            std::thread::sleep(Duration::from_millis(u64::from(options.settle_ms)));
            fixture.check_alive()?;
        }

        let clock = Arc::new(MasterClock::default());
        let mut session = RegionCaptureSession::start(
            &region,
            CaptureContext {
                clock: Arc::clone(&clock),
                cancellation: CaptureCancellation::default(),
            },
        )?;
        let prepared_ms = started.elapsed().as_millis();

        let first = match wait_for_first_crop(&mut session) {
            Ok(first) => first,
            Err(error) => {
                let _ = session.stop();
                return Err(error);
            }
        };
        if first.width != config.width || first.height != config.height {
            let error = invalid_error(format!(
                "The crop surface is {}x{} but the encoder was configured for {}x{}; a crop is never rescaled.",
                first.width, first.height, config.width, config.height
            ));
            let _ = session.stop();
            return Err(error);
        }
        let mut encoder = match WindowsH264Encoder::open(config, &first.surface) {
            Ok(encoder) => encoder,
            Err(error) => {
                let _ = session.stop();
                return Err(error);
            }
        };
        // The one explicit session origin: the first real source timestamp, unmodified.
        let origin = first.timestamp;
        let writer = match MkvVideoWriter::create(&output, encoder.info(), origin) {
            Ok(writer) => writer,
            Err(error) => {
                let _ = encoder.stop();
                let _ = session.stop();
                return Err(error);
            }
        };

        let mut owner = Owner {
            session,
            encoder,
            writer,
            lookahead: Some(first),
            pending: None,
            stop_requested: false,
            eos_requested: false,
            drain_started: None,
            last_progress: Instant::now(),
            last_marker: 0,
            stats: Stats {
                capture_frames: 1,
                prepared_ms,
                ..Stats::default()
            },
        };
        let record_until = Instant::now() + Duration::from_secs(u64::from(options.seconds));
        let outcome = loop {
            if !owner.stop_requested && Instant::now() >= record_until {
                owner.stop_requested = true;
            }
            match owner.poll() {
                Ok(progress) => {
                    if progress.drained {
                        break Ok(());
                    }
                    if progress.packets_written == 0 {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
                Err(error) => break Err(error),
            }
        };
        let mut stats = owner.stats;
        stats.drained_ms = owner
            .drain_started
            .map(|started| started.elapsed().as_millis())
            .unwrap_or(0);

        // Teardown order: hardware encoder first, capture second, container last. This is the
        // measured order for the shared Direct3D device the accepted evidence records.
        let encoder_failure = owner.encoder.stop().err();
        let capture_failure = owner.session.stop().err();
        let crop_diagnostics = owner
            .session
            .diagnostics()
            .unwrap_or_else(|error| format!("region diagnostics unavailable: {}", error.message));
        let encoder_diagnostics = owner.encoder.diagnostics();
        let cleanup_failure = combine_errors(encoder_failure, capture_failure);
        if let Err(mut error) = outcome {
            if let Some(cleanup) = cleanup_failure {
                error
                    .message
                    .push_str(&format!(" Cleanup also failed: {}", cleanup.message));
            }
            return Err(error);
        }
        if let Some(error) = cleanup_failure {
            return Err(error);
        }
        let artifact = owner.writer.finalize()?;
        if stats.accepted_frames == 0 || stats.packets_written == 0 || stats.keyframes == 0 {
            return Err(internal_error(format!(
                "The run delivered {} accepted frame(s), {} packet(s) and {} keyframe(s); an empty or keyframe-less stream is not evidence of a region recording.",
                stats.accepted_frames, stats.packets_written, stats.keyframes
            )));
        }
        drop(fixture);
        Ok(Completed {
            artifact: PathBuf::from(artifact.path),
            duration_ms: artifact.duration_ms,
            stats,
            crop_diagnostics,
            encoder_diagnostics,
            display_id: display.id.0.clone(),
            display_width: display.width,
            display_height: display.height,
            display_scale_factor: display.scale_factor,
            region,
        })
    }

    fn select_display<'a>(
        displays: &'a [DisplayInfo],
        options: &Options,
    ) -> Result<&'a DisplayInfo, RecorderError> {
        match &options.display_id {
            Some(id) => displays
                .iter()
                .find(|display| display.id.0.eq_ignore_ascii_case(id))
                .ok_or_else(|| {
                    invalid_error(format!(
                        "The requested display {id} is not attached; enumerated displays: {}.",
                        displays
                            .iter()
                            .map(|display| display.id.0.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                }),
            None if displays.len() == 1 => Ok(&displays[0]),
            None => Err(invalid_error(format!(
                "{} displays are attached; name the one to record with --display-id.",
                displays.len()
            ))),
        }
    }

    /// The band edge of the pattern window: 60 % of the display's physical height.
    fn band_height(display: &DisplayInfo) -> u32 {
        display.height / 10 * 6
    }

    /// The rectangle `--pattern` records, derived from the display and the pattern.
    fn pattern_region(display: &DisplayInfo) -> Result<Region, RecorderError> {
        let band = band_height(display);
        let y = i32::try_from(band.saturating_sub(PATTERN_BARS_ROWS)).unwrap_or(0);
        if band <= PATTERN_BARS_ROWS
            || y < 0
            || (y as u32).saturating_add(PATTERN_REGION_HEIGHT) > display.height
            || (PATTERN_REGION_X as u32).saturating_add(PATTERN_REGION_WIDTH) > display.width
        {
            return Err(invalid_error(format!(
                "The selected display is {}x{}: it is too small for the {PATTERN_REGION_WIDTH}x{PATTERN_REGION_HEIGHT} pattern rectangle that crosses a {band} px band. Use --region with an explicit rectangle instead.",
                display.width, display.height
            )));
        }
        Ok(Region {
            display_id: display.id.clone(),
            x: PATTERN_REGION_X,
            y,
            width: PATTERN_REGION_WIDTH,
            height: PATTERN_REGION_HEIGHT,
        })
    }

    fn wait_for_first_crop(
        session: &mut RegionCaptureSession,
    ) -> Result<CapturedFrame<WindowsSurface>, RecorderError> {
        let deadline = Instant::now() + PREPARATION_WATCHDOG;
        loop {
            if let Some(frame) = session.try_frame()? {
                return Ok(frame);
            }
            if Instant::now() >= deadline {
                return Err(internal_error(format!(
                    "The region session delivered no completed crop within {} ms.",
                    PREPARATION_WATCHDOG.as_millis()
                )));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn invalid_error(message: String) -> RecorderError {
        RecorderError {
            code: ErrorCode::InvalidRequest,
            message,
            recovery: RecoveryStatus::NoRecording,
            action: "Correct the probe request and run it again.".to_string(),
        }
    }

    fn internal_error(message: String) -> RecorderError {
        RecorderError {
            code: ErrorCode::Internal,
            message,
            recovery: RecoveryStatus::Unknown,
            action: "Discard the incomplete artifact and report the observed condition."
                .to_string(),
        }
    }

    /// Keeps the first teardown failure and appends the second, so neither is lost.
    fn combine_errors(
        first: Option<RecorderError>,
        second: Option<RecorderError>,
    ) -> Option<RecorderError> {
        match (first, second) {
            (Some(first), Some(second)) => Some(RecorderError {
                code: first.code,
                message: format!(
                    "{} Teardown also reported: {}",
                    first.message, second.message
                ),
                recovery: first.recovery,
                action: format!("{} Then: {}", first.action, second.action),
            }),
            (Some(first), None) => Some(first),
            (None, second) => second,
        }
    }

    /// What one bounded `poll` step did.
    #[derive(Clone, Copy, Debug, Default)]
    struct Progress {
        packets_written: u32,
        drained: bool,
    }

    /// The bounded recording owner: crop surfaces in, one Matroska file out.
    ///
    /// It adds no thread and no queue of its own: the capture worker, the crop pool and the
    /// encoder all already exist, and this example's control thread drives them.
    struct Owner {
        session: RegionCaptureSession,
        encoder: WindowsH264Encoder,
        writer: MkvVideoWriter,
        lookahead: Option<CapturedFrame<WindowsSurface>>,
        pending: Option<EncoderInput<CapturedFrame<WindowsSurface>>>,
        stop_requested: bool,
        eos_requested: bool,
        drain_started: Option<Instant>,
        last_progress: Instant,
        last_marker: u64,
        stats: Stats,
    }

    impl Owner {
        fn poll(&mut self) -> Result<Progress, RecorderError> {
            let mut progress = Progress::default();
            if !self.work_outstanding() {
                self.last_progress = Instant::now();
            }
            // 1. The writer consumes the encoder's bounded queue: deliver first, so a full
            //    queue never blocks admission for long.
            let (written, drained) = self.write_packets()?;
            progress.packets_written = written;
            if drained {
                if !self.eos_requested {
                    return Err(internal_error(
                        "The encoder reported a completed drain before end of stream was requested, so an owned input would be dropped."
                            .to_string(),
                    ));
                }
                progress.drained = true;
                return Ok(progress);
            }
            // 2. The pending input is advanced before any new surface is read, so
            //    backpressure pauses acquisition instead of growing memory.
            if self.pending.is_some() && !self.advance_pending()? {
                self.check_progress()?;
                return Ok(progress);
            }
            if self.stop_requested {
                // 3a. Drain: the last frame has no later source timestamp, so it receives the
                //     explicit nominal period; only then is end of stream requested.
                if self.drain_started.is_none() {
                    self.drain_started = Some(Instant::now());
                }
                if self.pending.is_none()
                    && let Some(last) = self.lookahead.take()
                {
                    let config = self.encoder.config();
                    self.pending = Some(EncoderInput {
                        input: last,
                        duration_ns: nominal_duration(config.rate_num, config.rate_den),
                    });
                    self.advance_pending()?;
                }
                if self.pending.is_none() && self.lookahead.is_none() && !self.eos_requested {
                    self.encoder.begin_drain()?;
                    self.eos_requested = true;
                }
                if self.eos_requested {
                    let (written, drained) = self.write_packets()?;
                    progress.packets_written += written;
                    progress.drained = drained;
                }
            } else if self.pending.is_none()
                && let Some(frame) = self.session.try_frame()?
            {
                // 3b. Recording: at most one crop surface per step, and none while an input
                //     is still pending.
                self.stats.capture_frames += 1;
                if let Some(previous) = self.lookahead.take() {
                    let duration = duration_until(previous.timestamp, frame.timestamp)?;
                    self.pending = Some(EncoderInput {
                        input: previous,
                        duration_ns: duration,
                    });
                    self.advance_pending()?;
                }
                self.lookahead = Some(frame);
            }
            self.check_progress()?;
            Ok(progress)
        }

        /// Writes at most the encoder's own bounded delivery queue.
        fn write_packets(&mut self) -> Result<(u32, bool), RecorderError> {
            let mut written = 0u32;
            while written < frame_windows::MAX_PACKETS as u32 {
                match self.encoder.try_packet()? {
                    EncoderPoll::Packet(packet) => {
                        let pts = packet.pts.0;
                        let bytes = packet.data.len() as u64;
                        let keyframe = packet.keyframe;
                        self.writer.write(packet)?;
                        written += 1;
                        self.stats.packets_written += 1;
                        self.stats.packet_bytes += bytes;
                        if keyframe {
                            self.stats.keyframes += 1;
                        }
                        self.stats.first_pts_ns.get_or_insert(pts);
                        self.stats.last_pts_ns = Some(pts);
                        let container = self.writer.stats();
                        self.stats.first_container_pts_ns = container.first_pts_ns;
                        self.stats.last_container_end_ns = container.last_end_ns;
                    }
                    EncoderPoll::Pending => break,
                    EncoderPoll::Drained => return Ok((written, true)),
                }
            }
            Ok((written, false))
        }

        /// Offers the pending input once; under pressure the same owned input stays pending.
        fn advance_pending(&mut self) -> Result<bool, RecorderError> {
            let Some(input) = self.pending.take() else {
                return Ok(true);
            };
            match self.encoder.submit(input)? {
                SubmitOutcome::Accepted => {
                    self.stats.accepted_frames += 1;
                    Ok(true)
                }
                SubmitOutcome::Backpressure(input) => {
                    self.stats.backpressure_returns += 1;
                    self.pending = Some(input);
                    Ok(false)
                }
            }
        }

        fn work_outstanding(&self) -> bool {
            if self.pending.is_some() {
                return true;
            }
            let counters = self.encoder.counters();
            counters.queued_inputs > 0
                || counters.submitted_in_flight() > 0
                || (self.eos_requested && !self.encoder.drained())
        }

        fn progress_marker(&self) -> u64 {
            let counters = self.encoder.counters();
            self.stats
                .packets_written
                .saturating_add(counters.accepted_inputs)
                .saturating_add(counters.native_submissions)
                .saturating_add(counters.native_buffered)
                .saturating_add(counters.packets_emitted)
                .saturating_add(counters.eos_acknowledgements)
        }

        /// The explicit progress bound: work is expected, so silence is reported, never
        /// waited out. A native call that never returns cannot be forcibly cancelled.
        fn check_progress(&mut self) -> Result<(), RecorderError> {
            let marker = self.progress_marker();
            if marker != self.last_marker {
                self.last_marker = marker;
                self.last_progress = Instant::now();
                return Ok(());
            }
            if !self.work_outstanding() {
                return Ok(());
            }
            let elapsed = self.last_progress.elapsed();
            if elapsed <= PROGRESS_WATCHDOG {
                return Ok(());
            }
            let counters = self.encoder.counters();
            Err(internal_error(format!(
                "The region recording made no observable progress for {} ms while it still owned work: {} accepted input(s), {} packet(s) written, {} packet(s) held, {} native submission(s) in flight. The last native call was: {}",
                elapsed.as_millis(),
                counters.accepted_inputs,
                self.stats.packets_written,
                counters.packets_held,
                counters.submitted_in_flight(),
                self.encoder.native_call_phase()
            )))
        }
    }
}

// The known-content window the `--pattern` mode records.
// Shared diagnostic fixture; the moving block keeps Desktop Duplication changing.
include!("support/pattern.rs");
