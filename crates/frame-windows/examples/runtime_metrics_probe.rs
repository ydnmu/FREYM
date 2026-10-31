//! Real capture/encode performance counters, read as numbers (FRAME-0203).
//!
//! It captures one real display, encodes the captured surfaces with the hardware H.264
//! encoder and writes the resulting Annex-B access units, so the counters can be compared
//! with the frames this run really took, really encoded and really wrote. Nothing here
//! parses a diagnostics report: every figure comes from the sessions' own typed read-only
//! stats (`WindowsCaptureSession::stats` and `WindowsH264Encoder::stats`), and the measured
//! rates come from `frame_metrics::PipelineMetrics`, which uses the existing `RateSampler`
//! per stage.
//!
//! ```text
//! cargo run --locked -p frame-windows --example runtime_metrics_probe -- \
//!     --display-id "<id from list_displays>" \
//!     --rate-num 60 --rate-den 1 \
//!     --seconds 3 \
//!     --output test-results/frame-0203-3s.h264
//! ```
//!
//! The run is bounded and it ends with a bounded **static window**: both sessions are
//! stopped, the rate baseline is reset explicitly, and one more sample over that real
//! window must measure `0.0` events per second for every stage. A zero there is an
//! observation, and it is reported as `Some(0.0)`, never as the `None` that means "not
//! measured".
//!
//! Options: `--display-id ID` (required), `--output PATH` (required, must not exist),
//! `--rate-num N` and `--rate-den N` (required: the explicit nominal encoder rate),
//! `--seconds N` (1..=3600, default 3) and `--sample-ms N` (50..=1000, default 250).
//!
//! The example polls the two sessions itself and holds no history: each sample is printed
//! and replaced, so the polling work stays bounded and allocation-light. It adds no
//! consumer, worker or timer to the media path.

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
    eprintln!("Native capture and hardware encoding are implemented for Windows only.");
    std::process::ExitCode::FAILURE
}

#[cfg(target_os = "windows")]
mod probe {
    use std::num::NonZeroU32;
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::time::{Duration, Instant};

    use frame_capture::{
        CaptureBackend, CaptureCancellation, CaptureContext, CaptureSession, CapturedFrame,
        DisplayBackend,
    };
    use frame_clock::MasterClock;
    use frame_contracts::{
        CaptureTarget, DisplayId, ErrorCode, FpsMode, RecorderError, RecoveryStatus,
    };
    use frame_encode::{
        EncoderInput, EncoderPoll, PacketEncoder, SubmitOutcome, duration_until, nominal_duration,
    };
    use frame_metrics::{
        PipelineCounters, PipelineMetrics, PipelineObservation, PipelineRates, QueueStats,
        RateSample,
    };
    use frame_windows::{
        CaptureSessionStats, NativeEncoderConfig, PlatformBackend, ProofArtifacts,
        WindowsCaptureSession, WindowsH264Encoder, WindowsSurface,
    };

    const USAGE: &str = "usage: runtime_metrics_probe --display-id ID --output PATH --rate-num N --rate-den N [--seconds 1..3600] [--sample-ms 50..1000]";
    const MAX_SECONDS: u32 = 3600;
    const DEFAULT_SECONDS: u32 = 3;
    const MIN_SAMPLE_MS: u32 = 50;
    const MAX_SAMPLE_MS: u32 = 1_000;
    const DEFAULT_SAMPLE_MS: u32 = 250;
    /// The trailing window in which no stage can produce anything: both sessions are
    /// stopped before it, so a rate measured across it is a real measured zero. It is
    /// longer than `frame_metrics::MIN_SAMPLE_WINDOW`, which is what makes the zero a
    /// measurement instead of a "not observed" result.
    const STATIC_WINDOW: Duration = Duration::from_millis(1_500);
    /// The same five-second native-progress budget the encoder worker uses: a driver call
    /// that stops completing work is reported instead of waited out.
    const PROGRESS_WATCHDOG: Duration = Duration::from_secs(5);
    /// A caller-side bound on the drain, so a library defect cannot hang the example.
    const DRAIN_BOUND: Duration = Duration::from_secs(30);
    const IDLE_PAUSE: Duration = Duration::from_millis(1);

    struct Options {
        display_id: Option<String>,
        output: Option<PathBuf>,
        rate_num: Option<u32>,
        rate_den: Option<u32>,
        seconds: u32,
        sample_ms: u32,
    }

    fn parse_options() -> Result<Options, String> {
        let arguments: Vec<String> = std::env::args().skip(1).collect();
        let mut options = Options {
            display_id: None,
            output: None,
            rate_num: None,
            rate_den: None,
            seconds: DEFAULT_SECONDS,
            sample_ms: DEFAULT_SAMPLE_MS,
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
                "--sample-ms" => {
                    let raw = take("--sample-ms")?;
                    let sample_ms: u32 = raw
                        .parse()
                        .map_err(|_| format!("--sample-ms expects a number, not {raw}"))?;
                    if !(MIN_SAMPLE_MS..=MAX_SAMPLE_MS).contains(&sample_ms) {
                        return Err(format!(
                            "--sample-ms must be {MIN_SAMPLE_MS}..={MAX_SAMPLE_MS}, not {sample_ms}"
                        ));
                    }
                    options.sample_ms = sample_ms;
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

    fn invalid_request(message: String) -> RecorderError {
        RecorderError {
            code: ErrorCode::InvalidRequest,
            message,
            recovery: RecoveryStatus::NoRecording,
            action: "Copy a display identity from the enumerated list and retry.".to_string(),
        }
    }

    fn internal_error(message: String) -> RecorderError {
        RecorderError {
            code: ErrorCode::Internal,
            message,
            recovery: RecoveryStatus::Unknown,
            action: "Do not treat this run as evidence; report the failure.".to_string(),
        }
    }

    /// One measured stage as `rate/s(events/window)`, or `not measured` for `None`.
    fn describe(sample: Option<RateSample>) -> String {
        match sample {
            Some(sample) => format!(
                "{:.3}/s({} events/{} ms)",
                sample.rate_hz,
                sample.events,
                sample.window.as_millis()
            ),
            None => "not measured".to_string(),
        }
    }

    /// The queue gauge, or the explicit reason there is none.
    fn describe_queue(queue: Option<QueueStats>) -> String {
        match queue {
            Some(queue) => format!(
                "depth={}/{} high_water={} evicted={}",
                queue.depth, queue.capacity, queue.high_water, queue.dropped
            ),
            None => "unavailable (contended or poisoned; no value substituted)".to_string(),
        }
    }

    /// Everything this run wrote.
    struct Written {
        packets: u64,
        bytes: u64,
    }

    /// Writes every ready packet to the stream, in order.
    fn drain_packets(
        encoder: &mut WindowsH264Encoder,
        artifacts: &mut ProofArtifacts,
        written: &mut Written,
    ) -> Result<bool, RecorderError> {
        loop {
            match encoder.try_packet()? {
                EncoderPoll::Packet(packet) => {
                    artifacts.write_access_unit(&packet.data)?;
                    written.packets += 1;
                    written.bytes += packet.data.len() as u64;
                }
                EncoderPoll::Pending => return Ok(false),
                EncoderPoll::Drained => return Ok(true),
            }
        }
    }

    fn wait_for_frame(
        session: &mut WindowsCaptureSession,
    ) -> Result<CapturedFrame<WindowsSurface>, RecorderError> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(frame) = session.try_frame()? {
                return Ok(frame);
            }
            if Instant::now() >= deadline {
                return Err(internal_error(
                    "The capture session delivered no completed surface within 5 s.".to_string(),
                ));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// One bounded sample: reads both sessions' typed stats and measures the stage rates.
    ///
    /// The encoded stage is fed only while the session's packet count and completed-frame
    /// count agree, so a packet count is never turned into an FPS behind the caller's back.
    fn sample(
        metrics: &mut PipelineMetrics,
        session: &WindowsCaptureSession,
        encoder: &WindowsH264Encoder,
        started: Instant,
    ) -> PipelineRates {
        let capture = session.stats();
        let encoder_stats = encoder.stats();
        metrics.observe(PipelineObservation {
            elapsed: started.elapsed(),
            counters: PipelineCounters {
                capture_updates: capture.counters.updates_with_image,
                capture_copies: capture.counters.copy_completions,
                captured_frames: capture.counters.dequeued,
                encoder_accepted: encoder_stats.accepted_inputs,
                encoder_completed_frames: if encoder_stats.packets_match_frames() {
                    Some(encoder_stats.completed_frames)
                } else {
                    None
                },
            },
            ready_queue: capture.ready_queue,
            nominal_source_hz: capture.nominal_source_hz,
        })
    }

    /// Whether one sample completed at least one real window, so it is a measurement and
    /// not only a new baseline.
    fn any_measured(rates: &PipelineRates) -> bool {
        rates.capture_update_rate.is_some()
            || rates.captured_fps.is_some()
            || rates.encoded_fps.is_some()
    }

    fn print_sample(label: &str, rates: &PipelineRates) {
        println!(
            "runtime sample {label}: elapsed_ms={} updates={} copies={} captured={} accepted={} encoded={} queue=[{}] nominal_hz={:?}",
            rates.observed_at.as_millis(),
            describe(rates.capture_update_rate),
            describe(rates.capture_copy_rate),
            describe(rates.captured_fps),
            describe(rates.encoder_accept_rate),
            describe(rates.encoded_fps),
            describe_queue(rates.ready_queue),
            rates.nominal_source_hz,
        );
    }

    pub(super) fn run() -> ExitCode {
        let options = match parse_options() {
            Ok(options) => options,
            Err(message) => {
                eprintln!("runtime_metrics_probe: {message}");
                eprintln!("{USAGE}");
                return ExitCode::FAILURE;
            }
        };
        let Some(display_id) = options.display_id.clone() else {
            eprintln!("runtime_metrics_probe: --display-id is required.");
            eprintln!("Currently enumerated displays:");
            eprint!("{}", available_displays());
            return ExitCode::FAILURE;
        };
        let Some(output) = options.output.clone() else {
            eprintln!("runtime_metrics_probe: --output is required and must not already exist.");
            eprintln!("{USAGE}");
            return ExitCode::FAILURE;
        };
        let (Some(rate_num), Some(rate_den)) = (options.rate_num, options.rate_den) else {
            eprintln!(
                "runtime_metrics_probe: --rate-num and --rate-den are required; the nominal encoder rate is explicit and is never derived from a measured source rate."
            );
            return ExitCode::FAILURE;
        };
        let (Some(rate_num), Some(rate_den)) =
            (NonZeroU32::new(rate_num), NonZeroU32::new(rate_den))
        else {
            eprintln!("runtime_metrics_probe: the nominal rate must be a positive rational.");
            return ExitCode::FAILURE;
        };

        match probe(
            &display_id,
            &output,
            rate_num,
            rate_den,
            options.seconds,
            options.sample_ms,
        ) {
            Ok(report) => {
                print!("{report}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("runtime_metrics_probe failed: {:?}", error.code);
                eprintln!("{}", error.message);
                eprintln!("action: {}", error.action);
                eprintln!(
                    "no artifact was produced by this run: {} was not completed by it.",
                    output.display()
                );
                ExitCode::FAILURE
            }
        }
    }

    fn probe(
        display_id: &str,
        output: &std::path::Path,
        rate_num: NonZeroU32,
        rate_den: NonZeroU32,
        seconds: u32,
        sample_ms: u32,
    ) -> Result<String, RecorderError> {
        let displays = PlatformBackend.enumerate_displays()?;
        let Some(display) = displays
            .iter()
            .find(|display| display.id.0 == display_id)
            .cloned()
        else {
            return Err(invalid_request(format!(
                "The requested display {display_id} is not attached; enumerated displays:\n{}",
                available_displays()
            )));
        };
        let config = NativeEncoderConfig::proof(display.width, display.height, rate_num, rate_den)?;
        let mut artifacts = ProofArtifacts::create(output)?;

        let clock = std::sync::Arc::new(MasterClock::default());
        let cancellation = CaptureCancellation::default();
        let mut backend = PlatformBackend;
        let mut session = backend.start(
            &CaptureTarget::Display(DisplayId(display_id.to_string())),
            FpsMode::AutoMatchSource,
            CaptureContext {
                clock,
                cancellation: cancellation.clone(),
            },
        )?;
        let first = match wait_for_frame(&mut session) {
            Ok(frame) => frame,
            Err(error) => {
                let _ = session.stop();
                return Err(error);
            }
        };
        let mut encoder = match WindowsH264Encoder::open(config, &first.surface) {
            Ok(encoder) => encoder,
            Err(error) => {
                let _ = session.stop();
                return Err(error);
            }
        };

        let mut metrics = PipelineMetrics::new();
        let started = Instant::now();
        // The last sample that completed a real window. A later sample inside the same
        // window is not a measurement, so it never replaces this one.
        let mut last_measured = sample(&mut metrics, &session, &encoder, started);
        let mut taken = 1u64; // `wait_for_frame` already took one frame
        let mut accepted = 0u64;
        let mut written = Written {
            packets: 0,
            bytes: 0,
        };
        let mut lookahead: Option<CapturedFrame<WindowsSurface>> = Some(first);
        let mut pending: Option<EncoderInput<CapturedFrame<WindowsSurface>>> = None;
        let mut last_sample = Instant::now();
        let mut last_progress = Instant::now();
        let mut progress_marker = 0u64;

        let outcome: Result<(), RecorderError> = (|| {
            let deadline = started + Duration::from_secs(u64::from(seconds));
            while Instant::now() < deadline {
                if drain_packets(&mut encoder, &mut artifacts, &mut written)? {
                    return Err(internal_error(
                        "The encoder drained before this run asked it to.".to_string(),
                    ));
                }
                if let Some(input) = pending.take() {
                    match encoder.submit(input)? {
                        SubmitOutcome::Accepted => accepted += 1,
                        SubmitOutcome::Backpressure(input) => pending = Some(input),
                    }
                }
                let stats = encoder.stats();
                let progress = stats
                    .accepted_inputs
                    .saturating_add(stats.completed_frames)
                    .saturating_add(stats.submitted_in_flight);
                if progress != progress_marker {
                    progress_marker = progress;
                    last_progress = Instant::now();
                }
                if (pending.is_some() || stats.submitted_in_flight > 0)
                    && last_progress.elapsed() > PROGRESS_WATCHDOG
                {
                    eprintln!(
                        "runtime_metrics_probe stalled: no native progress for {} ms; last native call: {}",
                        last_progress.elapsed().as_millis(),
                        encoder.native_call_phase()
                    );
                    // A native call that has not returned cannot be cancelled, so joining
                    // would block forever: report and terminate instead of pretending to
                    // have finished cleanly.
                    std::process::exit(1);
                }
                if pending.is_some() {
                    std::thread::sleep(IDLE_PAUSE);
                    continue;
                }
                if last_sample.elapsed() >= Duration::from_millis(u64::from(sample_ms)) {
                    last_sample = Instant::now();
                    let current = sample(&mut metrics, &session, &encoder, started);
                    if any_measured(&current) {
                        last_measured = current;
                    }
                    print_sample("run", &current);
                }
                match session.try_frame()? {
                    Some(frame) => {
                        taken += 1;
                        if let Some(previous) = lookahead.take() {
                            let duration = duration_until(previous.timestamp, frame.timestamp)?;
                            pending = Some(EncoderInput {
                                input: previous,
                                duration_ns: duration,
                            });
                        }
                        lookahead = Some(frame);
                    }
                    None => std::thread::sleep(IDLE_PAUSE),
                }
            }

            // The held lookahead frame is the last one, and it has no strictly later source
            // timestamp: it receives the explicit nominal period, exactly as the accepted
            // encoder proof does.
            if let Some(input) = pending.take() {
                submit_until_accepted(
                    &mut encoder,
                    &mut artifacts,
                    &mut written,
                    &mut accepted,
                    input,
                )?;
            }
            if let Some(last_frame) = lookahead.take() {
                submit_until_accepted(
                    &mut encoder,
                    &mut artifacts,
                    &mut written,
                    &mut accepted,
                    EncoderInput {
                        input: last_frame,
                        duration_ns: nominal_duration(config.rate_num, config.rate_den),
                    },
                )?;
            }
            // The encoder drains and stops before the capture session, so no capture worker
            // is left inside a native copy while NVENC still owns the shared device.
            encoder.begin_drain()?;
            let drain_deadline = Instant::now() + DRAIN_BOUND;
            loop {
                if drain_packets(&mut encoder, &mut artifacts, &mut written)? {
                    break;
                }
                if Instant::now() >= drain_deadline {
                    return Err(internal_error(format!(
                        "The drain did not complete within {} ms.",
                        DRAIN_BOUND.as_millis()
                    )));
                }
                std::thread::sleep(IDLE_PAUSE);
            }
            let _ = sample(&mut metrics, &session, &encoder, started);
            encoder.stop()?;
            session.stop()?;
            Ok(())
        })();

        if let Err(error) = outcome {
            let _ = encoder.stop();
            let _ = session.stop();
            return Err(error);
        }

        // The bounded static window: both sessions are stopped, so no counter can move.
        //
        // The rate baseline is reset explicitly first. Without that, the next sample would
        // still be averaged from the last window the run emitted, and a counter that moved
        // *before* the stop would be reported as a rate over a window that was supposed to
        // be static. After the reset the first sample only establishes the baseline and the
        // second one measures exactly the frozen window; the counters are compared before
        // and after it, so "static" is evidence and not an assumption.
        metrics.reset();
        let before_static = sample(&mut metrics, &session, &encoder, started);
        let before_static_counters = (session.stats().counters, encoder.stats());
        print_sample("static-baseline", &before_static);
        std::thread::sleep(STATIC_WINDOW);
        let after_static = sample(&mut metrics, &session, &encoder, started);
        let after_static_counters = (session.stats().counters, encoder.stats());
        print_sample("static-window", &after_static);

        let capture = session.stats();
        let encoder_stats = encoder.stats();
        let encoder_counters = encoder.counters();
        let identities = encoder_counters.identities();

        // The counters this run read are compared with what it really did. A mismatch fails
        // the run instead of being reported as a measurement.
        let mut checks: Vec<(&'static str, bool, String)> = Vec::new();
        checks.push((
            "capture_dequeued_equals_frames_taken",
            capture.counters.dequeued == taken,
            format!("counter={} taken={taken}", capture.counters.dequeued),
        ));
        checks.push((
            "capture_delivery_identity",
            capture_delivery_identity(&capture),
            format!(
                "copy_completions={} vs queue.depth+dequeued+evictions+contention+stop+verification+poison+cancellation+unwind={}",
                capture.counters.copy_completions,
                capture_delivery_sum(&capture)
            ),
        ));
        checks.push((
            "encoder_accepted_equals_submissions",
            encoder_stats.accepted_inputs == accepted,
            format!(
                "counter={} accepted={accepted}",
                encoder_stats.accepted_inputs
            ),
        ));
        checks.push((
            "packets_equal_completed_frames",
            encoder_stats.packets_match_frames(),
            format!(
                "packets_emitted={} completed_frames={}",
                encoder_stats.packets_emitted, encoder_stats.completed_frames
            ),
        ));
        checks.push((
            "written_access_units_equal_completed_frames",
            written.packets == encoder_stats.completed_frames,
            format!(
                "written={} completed_frames={}",
                written.packets, encoder_stats.completed_frames
            ),
        ));
        checks.push((
            "delivered_packets_all_written",
            encoder_stats.packets_held == 0
                && encoder_stats.packets_dequeued == encoder_stats.packets_emitted,
            format!(
                "dequeued={} emitted={} held={}",
                encoder_stats.packets_dequeued,
                encoder_stats.packets_emitted,
                encoder_stats.packets_held
            ),
        ));
        checks.push((
            "encoder_counters_reconcile",
            identities.is_ok(),
            match &identities {
                Ok(()) => "every checked identity holds".to_string(),
                Err(detail) => detail.clone(),
            },
        ));
        checks.push((
            "static_counters_unchanged",
            before_static_counters == after_static_counters,
            format!("before={before_static_counters:?} after={after_static_counters:?}"),
        ));
        checks.push((
            "static_baseline_is_a_new_baseline",
            !any_measured(&before_static),
            format!(
                "baseline sample after the reset: {}",
                static_summary(&before_static)
            ),
        ));
        checks.push((
            "static_window_measured_zero",
            static_rates_are_zero(&after_static),
            static_summary(&after_static),
        ));
        let failed: Vec<&str> = checks
            .iter()
            .filter(|(_, holds, _)| !holds)
            .map(|(name, _, _)| *name)
            .collect();
        if !failed.is_empty() {
            // The artifact is deliberately left on disk as explicitly incomplete: a failed
            // run is never reported as a proof.
            return Err(internal_error(format!(
                "The counted frames and the session counters disagree: {}. Details: {}",
                failed.join(", "),
                checks
                    .iter()
                    .map(|(name, holds, detail)| format!("{name}={holds}({detail})"))
                    .collect::<Vec<_>>()
                    .join("; ")
            )));
        }

        let mut report = String::new();
        report.push_str("Frame Windows runtime performance counters (FRAME-0203)\n");
        report.push_str(
            "Every figure below was read from a running native session's own typed counters; no diagnostics report was parsed, and no CPU or GPU utilization is estimated.\n",
        );
        report.push_str(&format!(
            "requested: display_id={display_id} seconds={seconds} sample_ms={sample_ms} output={} rate={rate_num}/{rate_den}\n",
            artifacts.stream_path().display()
        ));
        report.push_str(&format!(
            "source: geometry={}x{} enumerated_nominal_hz={:?} session_nominal_hz={:?}\n",
            display.width, display.height, display.refresh_rate_hz, capture.nominal_source_hz
        ));
        report.push_str(&format!(
            "capture_session: session_id={} stopped={} worker_finished={} queue=[{}]\n",
            capture.session_id,
            capture.stopped,
            capture.worker_finished,
            describe_queue(capture.ready_queue)
        ));
        report.push_str(&format!(
            "capture_counters: updates_with_image={} copy_submissions={} copy_completions={} dequeued={}\n",
            capture.counters.updates_with_image,
            capture.counters.copy_submissions,
            capture.counters.copy_completions,
            capture.counters.dequeued
        ));
        report.push_str(&format!(
            "capture_not_copied: repeats={} pre_epoch={} discontinuities={} unmappable={} pool_exhausted={} (mutually exclusive categories; do not add them to a delivery loss)\n",
            capture.counters.repeats,
            capture.counters.pre_epoch,
            capture.counters.discontinuities,
            capture.counters.unmappable,
            capture.counters.pool_exhausted
        ));
        report.push_str(&format!(
            "capture_copy_and_delivery_losses: copy_failures={} ready_evictions={} worker_contention_drops={} stop_discarded_pending={} stop_discarded_queued={} verification_discarded={} poison_discarded={} cancellation_discarded={} unwind_discarded_pending={} unwind_discarded_completed={}\n",
            capture.counters.copy_failures,
            capture.counters.ready_evictions,
            capture.counters.worker_contention_drops,
            capture.counters.stop_discarded_pending,
            capture.counters.stop_discarded_queued,
            capture.counters.verification_discarded,
            capture.counters.poison_discarded,
            capture.counters.cancellation_discarded,
            capture.counters.unwind_discarded_pending,
            capture.counters.unwind_discarded_completed
        ));
        report.push_str(&format!(
            "capture_other: upstream_accumulated_updates={} (an upstream counter, not a Frame drop) protected_content_masked={} (also counted in updates_with_image) consumer_contention={} (a try_lock miss, not a dropped frame)\n",
            capture.counters.upstream_accumulated_updates,
            capture.counters.protected_content_masked,
            capture.counters.consumer_contention
        ));
        report.push_str(&format!(
            "encoder_session: accepted_inputs={} backpressure_returns={} rejected_inputs={} completed_frames={} packets_emitted={} packets_dequeued={} packets_held={}/{}\n",
            encoder_stats.accepted_inputs,
            encoder_stats.backpressure_returns,
            encoder_stats.rejected_inputs,
            encoder_stats.completed_frames,
            encoder_stats.packets_emitted,
            encoder_stats.packets_dequeued,
            encoder_stats.packets_held,
            encoder_stats.packet_capacity
        ));
        report.push_str(&format!(
            "encoder_queues: queued_inputs={}/{} queue_high_water={} submitted_in_flight={} retained_leases={}/{} native_failures={} discarded_on_stop={} discarded_on_failure={} discarded_on_unwind={}\n",
            encoder_stats.queued_inputs,
            encoder_stats.ingress_capacity,
            encoder_stats.queue_high_water,
            encoder_stats.submitted_in_flight,
            encoder_stats.retained_leases,
            encoder_stats.lease_capacity,
            encoder_stats.native_failures,
            encoder_stats.discarded_on_stop,
            encoder_stats.discarded_on_failure,
            encoder_stats.discarded_on_unwind
        ));
        report.push_str(&format!(
            "written: access_units={} bytes={} stream={} sidecar={}\n",
            written.packets,
            written.bytes,
            artifacts.stream_path().display(),
            artifacts.sidecar_path().display()
        ));
        report.push_str(&format!(
            "last_measured_rates: elapsed_ms={} updates={} copies={} captured={} accepted={} encoded={} (the last sample that completed a real window; a later sub-window sample is not a measurement)\n",
            last_measured.observed_at.as_millis(),
            describe(last_measured.capture_update_rate),
            describe(last_measured.capture_copy_rate),
            describe(last_measured.captured_fps),
            describe(last_measured.encoder_accept_rate),
            describe(last_measured.encoded_fps)
        ));
        report.push_str(&format!(
            "static_window: window_ms={} counters_unchanged={} baseline_was_new={} updates={} copies={} captured={} accepted={} encoded={}\n",
            STATIC_WINDOW.as_millis(),
            before_static_counters == after_static_counters,
            !any_measured(&before_static),
            describe(after_static.capture_update_rate),
            describe(after_static.capture_copy_rate),
            describe(after_static.captured_fps),
            describe(after_static.encoder_accept_rate),
            describe(after_static.encoded_fps)
        ));
        report.push_str("reconcile:\n");
        for (name, holds, detail) in &checks {
            report.push_str(&format!(
                "  {name}={} ({detail})\n",
                if *holds { "ok" } else { "mismatch" }
            ));
        }
        report.push_str(&format!(
            "note: the hardware nominal rate ({:?} Hz) is a display mode value, and it is not the captured or the encoded rate; the measured captured and encoded rates above are separate observations of this run.\n",
            capture.nominal_source_hz
        ));
        artifacts.write_sidecar_line(&format!(
            "{{\"kind\":\"runtime_metrics\",\"accepted\":{},\"completed_frames\":{},\"packets_emitted\":{},\"written\":{},\"capture_dequeued\":{},\"static_window_ms\":{}}}",
            encoder_stats.accepted_inputs,
            encoder_stats.completed_frames,
            encoder_stats.packets_emitted,
            written.packets,
            capture.counters.dequeued,
            STATIC_WINDOW.as_millis()
        ))?;
        // A failed flush is a failed run: `finish` reports it instead of leaving a
        // half-written stream that looks complete.
        artifacts.finish()?;
        Ok(report)
    }

    /// Retries one owned input until the encoder accepts it, draining in between.
    fn submit_until_accepted(
        encoder: &mut WindowsH264Encoder,
        artifacts: &mut ProofArtifacts,
        written: &mut Written,
        accepted: &mut u64,
        input: EncoderInput<CapturedFrame<WindowsSurface>>,
    ) -> Result<(), RecorderError> {
        let mut pending = Some(input);
        let deadline = Instant::now() + PROGRESS_WATCHDOG;
        loop {
            if let Some(current) = pending.take() {
                match encoder.submit(current)? {
                    SubmitOutcome::Accepted => {
                        *accepted += 1;
                        return Ok(());
                    }
                    SubmitOutcome::Backpressure(current) => pending = Some(current),
                }
            }
            if drain_packets(encoder, artifacts, written)? {
                return Err(internal_error(
                    "The encoder drained before accepting its final input.".to_string(),
                ));
            }
            if Instant::now() >= deadline {
                return Err(internal_error(format!(
                    "The hardware encoder did not accept a final input within {} ms.",
                    PROGRESS_WATCHDOG.as_millis()
                )));
            }
            std::thread::sleep(IDLE_PAUSE);
        }
    }

    /// The capture delivery identity, evaluated from the typed counters alone.
    fn capture_delivery_identity(stats: &CaptureSessionStats) -> bool {
        stats.counters.copy_completions == capture_delivery_sum(stats)
    }

    fn capture_delivery_sum(stats: &CaptureSessionStats) -> u64 {
        let depth = stats.ready_queue.map_or(0, |queue| queue.depth as u64);
        depth
            + stats.counters.dequeued
            + stats.counters.ready_evictions
            + stats.counters.worker_contention_drops
            + stats.counters.stop_discarded_queued
            + stats.counters.verification_discarded
            + stats.counters.poison_discarded
            + stats.counters.cancellation_discarded
            + stats.counters.unwind_discarded_completed
    }

    /// True when every stage measured a real zero over the static window.
    fn static_rates_are_zero(rates: &PipelineRates) -> bool {
        [
            rates.capture_update_rate,
            rates.capture_copy_rate,
            rates.captured_fps,
            rates.encoder_accept_rate,
            rates.encoded_fps,
        ]
        .iter()
        .all(|sample| {
            sample.is_some_and(|sample| {
                sample.events == 0
                    && sample.rate_hz == 0.0
                    && sample.window >= frame_metrics::MIN_SAMPLE_WINDOW
            })
        })
    }

    fn static_summary(rates: &PipelineRates) -> String {
        format!(
            "updates={} copies={} captured={} accepted={} encoded={}",
            describe(rates.capture_update_rate),
            describe(rates.capture_copy_rate),
            describe(rates.captured_fps),
            describe(rates.encoder_accept_rate),
            describe(rates.encoded_fps)
        )
    }
}
