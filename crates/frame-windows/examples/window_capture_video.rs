//! One real window recorded straight into a single-video Matroska file (FRAME-0204).
//!
//! It drives the real Windows.Graphics.Capture window session, the real NVENC H.264
//! session and the accepted single-video Matroska writer, with the captured window's own
//! variable frame timestamps. It writes exactly one new file and nothing else: no sidecar,
//! no audio track, no UI binding and no IPC command.
//!
//! ```text
//! cargo run --locked -p frame-windows --example window_capture_video -- --list
//! cargo run --locked -p frame-windows --example window_capture_video -- \
//!     --window-id window:hwnd=0x000000000002049E;pid=18308 \
//!     --rate-num 60 --rate-den 1 --seconds 3 --output test-results/frame-0204-window.mkv
//! ```
//!
//! `--list` prints the real window list and exits. Without it, the selected window's
//! identity is re-verified, the session is started, and the run reports the container's
//! own packet count, byte size and duration after a completed finalization. The output
//! path must not exist. A failed run leaves no finalized artifact and says so.
//!
//! The selected window is never closed, moved or otherwise modified: this example only
//! reads it. `--seconds` is explicit and bounded, because the session is source-paced.

#[cfg(target_os = "windows")]
fn main() -> std::process::ExitCode {
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--frame-wgc-helper")
    {
        std::process::exit(frame_windows::run_wgc_helper());
    }
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--frame-mf-helper")
    {
        std::process::exit(frame_windows::run_mf_helper());
    }
    capture::run()
}

#[cfg(not(target_os = "windows"))]
fn main() -> std::process::ExitCode {
    eprintln!("Window capture is implemented for Windows only.");
    std::process::ExitCode::FAILURE
}

#[cfg(target_os = "windows")]
mod capture {
    use std::num::NonZeroU32;
    use std::path::{Path, PathBuf};
    use std::process::ExitCode;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use frame_capture::{CaptureCancellation, CaptureContext, CaptureSession, DisplayBackend};
    use frame_clock::MasterClock;
    use frame_contracts::{ErrorCode, FpsMode, RecorderError, RecoveryStatus, WindowId};
    use frame_encode::{
        EncoderInput, EncoderPoll, PacketEncoder, SubmitOutcome, duration_until, nominal_duration,
    };
    use frame_mux::{ContainerWriter, MkvVideoWriter};
    use frame_windows::{
        NativeEncoderConfig, PlatformBackend, WINDOW_FRAME_POOL_BUFFERS, WindowsH264Encoder,
        WindowsWindowCaptureSession,
    };

    const USAGE: &str = "usage: window_capture_video --list | --window-id ID --rate-num N --rate-den N --seconds 1..600 --output PATH";
    const MAX_SECONDS: u32 = 600;
    /// How long the first completed frame may take before the run reports a source that
    /// never produced one.
    const FIRST_FRAME_BOUND: Duration = Duration::from_secs(10);
    /// Caller-side bound on the drain, so a library defect cannot hang the example.
    const DRAIN_BOUND: Duration = Duration::from_secs(30);
    /// How the caller paces its own control loop. `try_frame` never sleeps itself.
    const IDLE_PAUSE: Duration = Duration::from_millis(1);

    struct Options {
        list: bool,
        window_id: Option<String>,
        rate_num: Option<u32>,
        rate_den: Option<u32>,
        seconds: Option<u32>,
        output: Option<PathBuf>,
    }

    fn parse_options() -> Result<Options, String> {
        let arguments: Vec<String> = std::env::args().skip(1).collect();
        let mut options = Options {
            list: false,
            window_id: None,
            rate_num: None,
            rate_den: None,
            seconds: None,
            output: None,
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
                "--list" => options.list = true,
                "--window-id" => options.window_id = Some(take("--window-id")?),
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
                    options.seconds = Some(seconds);
                }
                "--output" => options.output = Some(PathBuf::from(take("--output")?)),
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

    fn available_windows() -> String {
        match PlatformBackend.enumerate_windows() {
            Ok(windows) => windows
                .iter()
                .map(|window| {
                    format!(
                        "  {} pid={:?} title={:?}\n",
                        window.id.0, window.process_id, window.title
                    )
                })
                .collect(),
            Err(error) => format!("  (enumeration failed: {})\n", error.message),
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

    fn invalid_request(message: String) -> RecorderError {
        RecorderError {
            code: ErrorCode::InvalidRequest,
            message,
            recovery: RecoveryStatus::NoRecording,
            action: "Copy a window identity from the enumerated list and retry.".to_string(),
        }
    }

    /// Everything this run counted about the written container.
    #[derive(Default)]
    struct Written {
        packets: u64,
        bytes: u64,
        keyframes: u64,
        monotonic: bool,
        first_pts: Option<u64>,
        last_pts: Option<u64>,
    }

    /// Writes every ready packet into the container, in order.
    fn drain_packets<I>(
        encoder: &mut impl PacketEncoder<I>,
        writer: &mut MkvVideoWriter,
        written: &mut Written,
    ) -> Result<bool, RecorderError> {
        loop {
            match encoder.try_packet()? {
                EncoderPoll::Packet(packet) => {
                    if written.packets == 0 {
                        written.first_pts = Some(packet.pts.0);
                        written.monotonic = true;
                    } else if let Some(previous) = written.last_pts
                        && packet.pts.0 <= previous
                    {
                        written.monotonic = false;
                    }
                    written.last_pts = Some(packet.pts.0);
                    written.packets += 1;
                    written.bytes += packet.data.len() as u64;
                    if packet.keyframe {
                        written.keyframes += 1;
                    }
                    writer.write(packet)?;
                }
                EncoderPoll::Pending => return Ok(false),
                EncoderPoll::Drained => return Ok(true),
            }
        }
    }

    /// Submits one owned input, honouring the encoder's bounded backpressure.
    fn submit_until_accepted<I>(
        encoder: &mut impl PacketEncoder<EncoderInput<I>>,
        writer: &mut MkvVideoWriter,
        written: &mut Written,
        mut input: EncoderInput<I>,
    ) -> Result<(), RecorderError> {
        loop {
            match encoder.submit(input)? {
                SubmitOutcome::Accepted => return Ok(()),
                SubmitOutcome::Backpressure(returned) => {
                    input = returned;
                    drain_packets(encoder, writer, written)?;
                    std::thread::sleep(IDLE_PAUSE);
                }
            }
        }
    }

    /// Waits for the first completed surface, bounded and explicit.
    fn wait_for_frame(
        session: &mut WindowsWindowCaptureSession,
    ) -> Result<frame_capture::CapturedFrame<frame_windows::WindowsSurface>, RecorderError> {
        let deadline = Instant::now() + FIRST_FRAME_BOUND;
        loop {
            match session.try_frame()? {
                Some(frame) => return Ok(frame),
                None => {
                    if Instant::now() >= deadline {
                        return Err(internal_error(format!(
                            "The selected window produced no captured frame within {} ms. A hidden or minimized window publishes no new image; show the window and retry.",
                            FIRST_FRAME_BOUND.as_millis()
                        )));
                    }
                    std::thread::sleep(IDLE_PAUSE);
                }
            }
        }
    }

    pub(super) fn run() -> ExitCode {
        let options = match parse_options() {
            Ok(options) => options,
            Err(message) => {
                eprintln!("window_capture_video: {message}");
                eprintln!("{USAGE}");
                return ExitCode::FAILURE;
            }
        };
        if options.list {
            match frame_windows::window_diagnostics() {
                Ok(text) => {
                    print!("{text}");
                    println!("canonical list (DisplayBackend::enumerate_windows):");
                    print!("{}", available_windows());
                    return ExitCode::SUCCESS;
                }
                Err(error) => {
                    eprintln!("window_capture_video: {}", error.message);
                    return ExitCode::FAILURE;
                }
            }
        }
        let Some(window_id) = options.window_id.clone() else {
            eprintln!("window_capture_video: --window-id is required (or use --list).");
            eprintln!("Currently enumerated windows:");
            eprint!("{}", available_windows());
            return ExitCode::FAILURE;
        };
        let Some(output) = options.output.clone() else {
            eprintln!("window_capture_video: --output is required and must not already exist.");
            eprintln!("{USAGE}");
            return ExitCode::FAILURE;
        };
        let (Some(rate_num), Some(rate_den)) = (options.rate_num, options.rate_den) else {
            eprintln!(
                "window_capture_video: --rate-num and --rate-den are required; the nominal encoder rate is explicit and is never derived from a measured source rate."
            );
            return ExitCode::FAILURE;
        };
        let (Some(rate_num), Some(rate_den)) =
            (NonZeroU32::new(rate_num), NonZeroU32::new(rate_den))
        else {
            eprintln!("window_capture_video: the nominal rate must be a positive rational.");
            return ExitCode::FAILURE;
        };
        let Some(seconds) = options.seconds else {
            eprintln!(
                "window_capture_video: --seconds is required; this run is explicitly bounded."
            );
            eprintln!("{USAGE}");
            return ExitCode::FAILURE;
        };

        match record(&window_id, &output, rate_num, rate_den, seconds) {
            Ok(report) => {
                print!("{report}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("window_capture_video failed: {:?}", error.code);
                eprintln!("{}", error.message);
                eprintln!("action: {}", error.action);
                eprintln!(
                    "no finalized artifact was produced for this run: {}",
                    output.display()
                );
                ExitCode::FAILURE
            }
        }
    }

    fn record(
        window_id: &str,
        output: &Path,
        rate_num: NonZeroU32,
        rate_den: NonZeroU32,
        seconds: u32,
    ) -> Result<String, RecorderError> {
        let windows = PlatformBackend.enumerate_windows()?;
        let Some(window) = windows
            .iter()
            .find(|window| window.id.0 == window_id)
            .cloned()
        else {
            return Err(invalid_request(format!(
                "The requested window {window_id} is not on the current desktop; enumerated windows:\n{}",
                available_windows()
            )));
        };

        // One shared session clock and one shared origin for the whole recording: the
        // capture session maps the window's frame times onto it, and the container stores
        // timestamps relative to it.
        let clock = Arc::new(MasterClock::default());
        let origin = clock.now();
        let cancellation = CaptureCancellation::default();
        let mut session = WindowsWindowCaptureSession::start_diagnostic(
            &WindowId(window_id.to_string()),
            FpsMode::AutoMatchSource,
            CaptureContext {
                clock: Arc::clone(&clock),
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
        let config = match NativeEncoderConfig::proof(first.width, first.height, rate_num, rate_den)
        {
            Ok(config) => config,
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
        let settings = encoder.settings().clone();
        let mut writer = match MkvVideoWriter::create(output, encoder.info(), origin) {
            Ok(writer) => writer,
            Err(error) => {
                let _ = encoder.stop();
                let _ = session.stop();
                return Err(error);
            }
        };

        let started = Instant::now();
        let deadline = started + Duration::from_secs(u64::from(seconds));
        let mut written = Written::default();
        // At most one not-yet-submitted frame: the lookahead that gives the previous
        // frame its exact duration. The capture pool's bound is the ultimate limit.
        let mut lookahead = Some(first);
        let mut pending: Option<EncoderInput<_>> = None;

        let outcome = (|| -> Result<(), RecorderError> {
            loop {
                if let Some(input) = pending.take() {
                    match encoder.submit(input)? {
                        SubmitOutcome::Accepted => {}
                        SubmitOutcome::Backpressure(input) => {
                            pending = Some(input);
                        }
                    }
                }
                if pending.is_none() {
                    drain_packets(&mut encoder, &mut writer, &mut written)?;
                }
                if pending.is_some() {
                    std::thread::sleep(IDLE_PAUSE);
                    continue;
                }
                if Instant::now() >= deadline {
                    break;
                }
                match session.try_frame()? {
                    Some(frame) => {
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

            // The final frame has no strictly later source timestamp, so it receives the
            // explicit nominal period rounded up to nanoseconds.
            if let Some(input) = pending.take() {
                submit_until_accepted(&mut encoder, &mut writer, &mut written, input)?;
            }
            if let Some(last) = lookahead.take() {
                submit_until_accepted(
                    &mut encoder,
                    &mut writer,
                    &mut written,
                    EncoderInput {
                        input: last,
                        duration_ns: nominal_duration(config.rate_num, config.rate_den),
                    },
                )?;
            }

            // The encoder drains and stops before the capture session, because stopping
            // capture while an NVENC session is still encoding on the same D3D11 device can
            // leave the capture worker inside its copy path for an unbounded time.
            encoder.begin_drain()?;
            let drain_deadline = Instant::now() + DRAIN_BOUND;
            while !drain_packets(&mut encoder, &mut writer, &mut written)? {
                if Instant::now() >= drain_deadline {
                    return Err(internal_error(format!(
                        "The encoder did not drain within {} ms.",
                        DRAIN_BOUND.as_millis()
                    )));
                }
                std::thread::sleep(IDLE_PAUSE);
            }
            encoder.stop()?;
            session.stop()?;
            Ok(())
        })();

        if let Err(error) = outcome {
            // A failed run releases every resource and never finalizes the container, so
            // the partial file stays explicitly incomplete.
            let mut reported = error;
            if let Err(cleanup) = encoder.stop() {
                reported.message = format!(
                    "{} Cleanup also reported: {}",
                    reported.message, cleanup.message
                );
            }
            if let Err(cleanup) = session.stop() {
                reported.message = format!(
                    "{} Cleanup also reported: {}",
                    reported.message, cleanup.message
                );
            }
            drop(writer);
            return Err(reported);
        }

        let counters = encoder.counters();
        let identities = counters.identities();
        let stats = writer.stats();
        let artifact = writer.finalize()?;
        let bytes = std::fs::metadata(&artifact.path)
            .map_err(|error| {
                internal_error(format!(
                    "The finalized container {} could not be inspected: {error}",
                    artifact.path
                ))
            })?
            .len();
        if bytes == 0 || Path::new(&artifact.path) != output {
            return Err(internal_error(format!(
                "The finalized container {} is empty or is not the requested output {}.",
                artifact.path,
                output.display()
            )));
        }

        let elapsed = started.elapsed();
        let diagnostics = session.diagnostics()?;
        let mut report = String::new();
        report.push_str("Frame Windows window capture recording (FRAME-0204)\n");
        report.push_str(
            "This run records one real top-level window into one new single-video Matroska file: no audio track, no sidecar, no UI binding and no IPC command.\n",
        );
        report.push_str(&format!(
            "requested: window_id={} seconds={} output={} rate={}/{} origin_ns={} frame_pool_buffers={}\n",
            window_id,
            seconds,
            output.display(),
            rate_num,
            rate_den,
            origin.0,
            WINDOW_FRAME_POOL_BUFFERS
        ));
        report.push_str(&format!(
            "selected_window: pid={:?} title={:?} (the title is information; the identity is the handle plus its process)\n",
            window.process_id, window.title
        ));
        report.push_str(&format!(
            "source: geometry={}x{} capture=Window/AutoMatchSource native_pixel_format=DXGI_FORMAT_B8G8R8A8_UNORM\n",
            config.width, config.height
        ));
        report.push_str(&format!(
            "encoder: {}x{} at {}/{} nominal, CBR {} bit/s, GOP {}, {} initialization bytes\n",
            settings.width,
            settings.height,
            settings.rate_num,
            settings.rate_den,
            settings.bitrate_bps,
            settings.gop_length,
            settings.codec_config.len()
        ));
        report.push_str(&format!(
            "artifact: path={} bytes={bytes} duration_ms={:.3} packets={} keyframes={} first_container_pts_ns={:?} last_container_end_ns={}\n",
            artifact.path,
            artifact.duration_ms,
            stats.packets,
            stats.keyframes,
            stats.first_pts_ns,
            stats.last_end_ns
        ));
        report.push_str(&format!(
            "delivery: accepted_inputs={} backpressure_returns={} packets_emitted={} packets_dequeued={} written_packets={} written_bytes={} monotonic_timestamps={} first_pts_ns={:?} last_pts_ns={:?}\n",
            counters.accepted_inputs,
            counters.backpressure_returns,
            counters.packets_emitted,
            counters.packets_dequeued,
            written.packets,
            written.bytes,
            written.monotonic,
            written.first_pts,
            written.last_pts
        ));
        if let Err(detail) = identities {
            return Err(internal_error(format!(
                "The encoder counters do not reconcile after a completed drain: {detail}"
            )));
        }
        if written.packets == 0 {
            return Err(internal_error(
                "The encode produced no access unit, so there is no recording to report."
                    .to_string(),
            ));
        }
        report.push_str(&format!(
            "timing: total_ms={} container_retained_bytes={}\n",
            elapsed.as_millis(),
            stats.retained_bytes_estimate
        ));
        report.push_str(&format!(
            "audio_tracks: {} (this run records video only)\n",
            artifact.audio_tracks
        ));
        report.push('\n');
        report.push_str(&diagnostics);
        Ok(report)
    }
}
