// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Diagnostic Windows encoded-audio probe (FRAME-0107F).
//!
//! Runs the accepted live audio path against a *real* selected Windows source: one
//! `AudioPipeline` fed by `PlatformBackend` capture sessions, connected to one
//! `EncodedAudioPipeline` through the **same** `Arc<MasterClock>` and the **same** media
//! origin, exactly as a recording does. Nothing new is implemented here: no container, no
//! FFI, no worker, no second pipeline and no mux. With `--normalize` the accepted
//! FRAME-0206 `NormalizingBackend` is inserted between capture and the pipeline, so a
//! mono/stereo 44.1 or 96 kHz source reaches the same accepted 48 kHz line; without it the
//! raw native session is unchanged, exactly as before.
//!
//! Every completed Opus packet is written to its own per-track file as it flows, and one
//! JSONL index record per packet is appended at the same time, so the whole recording is
//! never held in memory. After a graceful drain the probe reads the written packets back
//! from disk and decodes them with the real libopus decoder to check what the accepted
//! trim metadata claims: after pre-skip and discard padding the decoded frame count equals
//! the encoder's accepted `input_frames`, and (when one source feeds both the main mix and
//! its clean stem) both tracks carry the same content.
//!
//! Capture happens ONLY when `--source ID=ROUTE`, `--seconds` and `--output-dir` are all
//! given explicitly. `--list`, a bare invocation and any partial invocation never record.
//!
//! ```text
//! cargo run --locked -p frame-windows --example encoded_audio_probe -- \
//!     --source "wasapi-mic:{0.0.0.00000000}.{...}=both" \
//!     --seconds 3 --output-dir test-results/frame-0107f/proof-mic-3s
//! ```
//!
//! Options: `--list` (enumerate capturable sources, never records), `--source ID=ROUTE`
//! (repeatable; `ROUTE` is `main`, `separate` or `both`; `ignore` is refused because the
//! probe never enables an ignored route), `--normalize` (optional; connect the source to
//! the accepted 48 kHz line through the FRAME-0206 adapter instead of feeding the raw
//! native session to the mixer), `--seconds N` (1..=10) and `--output-dir DIR`.

#[cfg(target_os = "windows")]
fn main() -> std::process::ExitCode {
    probe::run()
}

#[cfg(not(target_os = "windows"))]
fn main() -> std::process::ExitCode {
    eprintln!("The encoded audio probe needs Windows native capture.");
    std::process::ExitCode::FAILURE
}

#[cfg(target_os = "windows")]
mod probe {
    use frame_audio::{
        AudioCaptureBackend, AudioPipeline, DrainableAudioSession, NormalizingBackend, TrackKind,
        TrackPlan,
    };
    use frame_clock::{MasterClock, MediaTimestamp};
    use frame_contracts::{
        AudioRoute, AudioRouteSelection, AudioSourceId, ErrorCode, RecorderError, RecoveryStatus,
    };
    use frame_encode::EncodedPacket;
    use frame_encode::audio_pipeline::{EncodedAudioDrainPoll, EncodedAudioPipeline};
    use frame_windows::PlatformBackend;
    use serde::{Deserialize, Serialize};
    use std::fs::File;
    use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
    use std::path::{Path, PathBuf};
    use std::process::ExitCode;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const USAGE: &str = "usage: encoded_audio_probe --list | --source ID=ROUTE [--source ID=ROUTE ...] [--normalize] --seconds N --output-dir DIR";
    /// The probe is a short connection check, not a stress run.
    const MIN_SECONDS: u32 = 1;
    const MAX_SECONDS: u32 = 10;
    /// The accepted mixer route ceiling; the probe offers no route beyond it.
    const MAX_SOURCES: usize = 16;
    /// Bounded consumer pacing: one millisecond between nonblocking polls.
    const POLL_SLEEP: Duration = Duration::from_millis(1);
    /// Bounded graceful drain: a stuck worker is an explicit failure, never a hang.
    const DRAIN_WATCHDOG: Duration = Duration::from_secs(10);
    const OPUS_RATE: u32 = 48_000;
    const OPUS_CHANNELS: usize = 2;
    /// One Opus packet is at most 120 ms, i.e. 5760 frames per channel.
    const MAX_PACKET_FRAMES: usize = 5_760;
    const PACKETS_DIR: &str = "packets";
    const INDEX_NAME: &str = "index.jsonl";
    const REPORT_NAME: &str = "report.txt";
    const PROBE_NAME: &str = "encoded.probe.json";

    /// One explicitly selected source and the route the mixer will freeze for it.
    #[derive(Clone)]
    struct SelectedRoute {
        source_id: String,
        route: AudioRoute,
        label: &'static str,
    }

    struct Options {
        list: bool,
        sources: Vec<SelectedRoute>,
        normalize: bool,
        seconds: Option<u32>,
        output_dir: Option<PathBuf>,
    }

    /// `ID=ROUTE`, split at the last `=` so an identity containing `=` still parses.
    fn parse_source(raw: &str) -> Result<SelectedRoute, String> {
        let Some((source_id, route)) = raw.rsplit_once('=') else {
            return Err(format!("--source expects ID=ROUTE, not {raw}"));
        };
        if source_id.is_empty() {
            return Err(format!("--source {raw} has an empty source ID"));
        }
        let (route, label) = match route {
            "main" => (AudioRoute::MainMix, "main"),
            "separate" => (AudioRoute::SeparateOnly, "separate"),
            "both" => (AudioRoute::MainMixAndSeparate, "both"),
            // An ignored route is never a probe target: it opens no session and encodes
            // no track, so it could not prove the connection.
            "ignore" => {
                return Err(
                    "--source route ignore is refused: the probe never enables an ignored route; use main, separate or both"
                        .to_string(),
                );
            }
            other => {
                return Err(format!(
                    "--source route {other} is unknown; use main, separate or both"
                ));
            }
        };
        Ok(SelectedRoute {
            source_id: source_id.to_string(),
            route,
            label,
        })
    }

    fn parse_options(arguments: impl IntoIterator<Item = String>) -> Result<Options, String> {
        let mut options = Options {
            list: false,
            sources: Vec::new(),
            normalize: false,
            seconds: None,
            output_dir: None,
        };
        let mut arguments = arguments.into_iter();
        while let Some(argument) = arguments.next() {
            let mut take = |flag: &str| -> Result<String, String> {
                arguments
                    .next()
                    .ok_or_else(|| format!("{flag} needs a value"))
            };
            match argument.as_str() {
                "--list" => options.list = true,
                "--source" => {
                    let raw = take("--source")?;
                    options.sources.push(parse_source(&raw)?);
                }
                // FRAME-0206: opt-in connection of the real native format to the accepted
                // 48 kHz line. Without it the raw native session is used unchanged.
                "--normalize" => options.normalize = true,
                "--seconds" => {
                    let raw = take("--seconds")?;
                    let seconds: u32 = raw
                        .parse()
                        .map_err(|_| format!("--seconds expects a number, not {raw}"))?;
                    if !(MIN_SECONDS..=MAX_SECONDS).contains(&seconds) {
                        return Err(format!(
                            "--seconds must be {MIN_SECONDS}..={MAX_SECONDS}, not {seconds}"
                        ));
                    }
                    options.seconds = Some(seconds);
                }
                "--output-dir" => options.output_dir = Some(PathBuf::from(take("--output-dir")?)),
                "--help" | "-h" => {
                    println!("{USAGE}");
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument {other}")),
            }
        }
        if options.sources.len() > MAX_SOURCES {
            return Err(format!(
                "at most {MAX_SOURCES} sources are accepted, not {}",
                options.sources.len()
            ));
        }
        Ok(options)
    }

    fn output_error(path: &Path, error: &std::io::Error) -> RecorderError {
        let code = if error.kind() == std::io::ErrorKind::StorageFull {
            ErrorCode::DiskFull
        } else {
            ErrorCode::InvalidOutputPath
        };
        let detail = if error.kind() == std::io::ErrorKind::AlreadyExists {
            "already exists, and this probe never overwrites evidence"
        } else {
            "could not be written"
        };
        RecorderError {
            code,
            message: format!("The probe artifact {} {detail}: {error}.", path.display()),
            recovery: RecoveryStatus::NoRecording,
            action: "Choose a fresh output directory under the ignored test-results/ tree."
                .to_string(),
        }
    }

    fn internal(message: String) -> RecorderError {
        RecorderError {
            code: ErrorCode::Internal,
            message,
            recovery: RecoveryStatus::NoRecording,
            action: "This is a defect in the probe or in the connected pipeline.".to_string(),
        }
    }

    /// Keeps the first failure and appends a later, different one as context.
    fn retain_failure(primary: &mut Option<RecorderError>, later: RecorderError, phase: &str) {
        match primary {
            Some(primary) => {
                if primary.message != later.message {
                    primary
                        .message
                        .push_str(&format!(" {phase} also failed: {}", later.message));
                }
            }
            None => *primary = Some(later),
        }
    }

    /// Opens one path exclusively, refusing to overwrite existing evidence.
    fn open_exclusive(path: &Path) -> Result<File, RecorderError> {
        File::options()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| output_error(path, &error))
    }

    /// The phase a packet was produced in; the probe keeps the counts apart.
    #[derive(Clone, Copy)]
    enum Phase {
        Live,
        Drain,
        Finish,
    }

    impl Phase {
        fn label(self) -> &'static str {
            match self {
                Self::Live => "live",
                Self::Drain => "drain",
                Self::Finish => "finish",
            }
        }
    }

    /// One packet's index record: the only per-packet metadata kept, and it is written to
    /// disk as the packet flows instead of being accumulated in memory.
    #[derive(Debug, Serialize, Deserialize)]
    struct PacketRecord {
        seq: u64,
        track_id: u32,
        index: u64,
        phase: String,
        pts_ns: u64,
        dts_ns: u64,
        duration_ns: u64,
        keyframe: bool,
        offset: u64,
        size: u64,
    }

    /// One track's exclusive packet file plus its streamed counters.
    struct TrackWriter {
        path: PathBuf,
        file: BufWriter<File>,
        packets: u64,
        bytes: u64,
        live: u64,
        drain: u64,
        finish: u64,
        first_pts: Option<MediaTimestamp>,
        last_pts: Option<MediaTimestamp>,
    }

    /// The streaming sink: one file per frozen track and one JSONL index, all opened
    /// before the first native session starts.
    struct Artifacts {
        index_path: PathBuf,
        index: BufWriter<File>,
        tracks: Vec<TrackWriter>,
        seq: u64,
        packets: u64,
        bytes: u64,
    }

    impl Artifacts {
        fn open(output_dir: &Path, plan: &[TrackPlan]) -> Result<Self, RecorderError> {
            let packets_dir = output_dir.join(PACKETS_DIR);
            std::fs::create_dir_all(&packets_dir)
                .map_err(|error| output_error(&packets_dir, &error))?;
            let index_path = packets_dir.join(INDEX_NAME);
            let index = open_exclusive(&index_path)?;
            let mut tracks = Vec::with_capacity(plan.len());
            for track_id in 0..plan.len() {
                let path = packets_dir.join(format!("track-{track_id}.opus.bin"));
                tracks.push(TrackWriter {
                    file: BufWriter::new(open_exclusive(&path)?),
                    path,
                    packets: 0,
                    bytes: 0,
                    live: 0,
                    drain: 0,
                    finish: 0,
                    first_pts: None,
                    last_pts: None,
                });
            }
            Ok(Self {
                index: BufWriter::new(index),
                index_path,
                tracks,
                seq: 0,
                packets: 0,
                bytes: 0,
            })
        }

        /// Writes one packet's bytes and its index record immediately.
        fn write(&mut self, packet: &EncodedPacket, phase: Phase) -> Result<(), RecorderError> {
            let track_id = usize::try_from(packet.track_id)
                .map_err(|_| internal("A packet track id does not fit the plan index.".into()))?;
            let Some(track) = self.tracks.get_mut(track_id) else {
                return Err(internal(format!(
                    "A packet arrived for unplanned track {}.",
                    packet.track_id
                )));
            };
            let offset = track.bytes;
            let size = packet.data.len() as u64;
            track
                .file
                .write_all(&packet.data)
                .map_err(|error| output_error(&track.path, &error))?;
            let record = PacketRecord {
                seq: self.seq,
                track_id: packet.track_id,
                index: track.packets,
                phase: phase.label().to_string(),
                pts_ns: packet.pts.0,
                dts_ns: packet.dts.0,
                duration_ns: packet.duration_ns,
                keyframe: packet.keyframe,
                offset,
                size,
            };
            serde_json::to_writer(&mut self.index, &record).map_err(|error| {
                internal(format!(
                    "The packet index record could not be serialized: {error}"
                ))
            })?;
            self.index
                .write_all(b"\n")
                .map_err(|error| output_error(&self.index_path, &error))?;
            track.packets += 1;
            track.bytes += size;
            match phase {
                Phase::Live => track.live += 1,
                Phase::Drain => track.drain += 1,
                Phase::Finish => track.finish += 1,
            }
            if track.first_pts.is_none() {
                track.first_pts = Some(packet.pts);
            }
            track.last_pts = Some(packet.pts);
            self.seq += 1;
            self.packets += 1;
            self.bytes += size;
            Ok(())
        }

        /// Writes one batch of packets in the order the pipeline reported them.
        fn write_batch(
            &mut self,
            packets: &[EncodedPacket],
            phase: Phase,
        ) -> Result<(), RecorderError> {
            for packet in packets {
                self.write(packet, phase)?;
            }
            Ok(())
        }

        fn flush(&mut self) -> Result<(), RecorderError> {
            for track in &mut self.tracks {
                track
                    .file
                    .flush()
                    .map_err(|error| output_error(&track.path, &error))?;
            }
            self.index
                .flush()
                .map_err(|error| output_error(&self.index_path, &error))
        }
    }

    /// The run's separated evidence: packet phases, drain closure and the accepted
    /// per-track trim metadata.
    #[derive(Default)]
    struct RunEvidence {
        live_polls: u64,
        drain_polls: u64,
        drain_pending: u64,
        drain_requested: bool,
        drained: bool,
        watchdog_exceeded: bool,
        drain_ms: u128,
        elapsed_ms: u128,
        stop_result: Option<String>,
        failure_phase: Option<&'static str>,
        failure: Option<RecorderError>,
    }

    /// One planned track's accepted trim metadata, taken from the one-shot finish report.
    #[derive(Clone, Copy)]
    struct TrackTrim {
        pre_skip: u16,
        discard_padding: u16,
        input_frames: u64,
    }

    fn track_label(plan: &TrackPlan) -> String {
        match &plan.kind {
            TrackKind::MainMix => "mainMix".to_string(),
            TrackKind::Clean(id) => format!("clean:{}", id.0),
        }
    }

    /// Lists every capturable source so an operator copies a real identity and route.
    fn list_sources() -> Result<String, RecorderError> {
        use frame_audio::AudioBackend;
        let sources = PlatformBackend.enumerate_audio_sources()?;
        let mut text = format!("capturable sources: {}\n", sources.len());
        for source in &sources {
            text.push_str(&format!(
                "  {} — {:?} \"{}\" active={} route={:?}\n",
                source.id.0, source.kind, source.display_name, source.is_active, source.route
            ));
        }
        if sources.is_empty() {
            text.push_str(
                "  (no capturable source was reported; this is an honest empty result, not a fallback)\n",
            );
        }
        text.push_str("record with: --source ID=ROUTE, where ROUTE is main, separate or both\n");
        Ok(text)
    }

    pub(super) fn run() -> ExitCode {
        let options = match parse_options(std::env::args().skip(1)) {
            Ok(options) => options,
            Err(message) => {
                eprintln!("encoded_audio_probe: {message}");
                eprintln!("{USAGE}");
                return ExitCode::FAILURE;
            }
        };

        if options.list {
            return match list_sources() {
                Ok(text) => {
                    print!("{text}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("encoded_audio_probe failed: {:?}", error.code);
                    eprintln!("{}", error.message);
                    eprintln!("action: {}", error.action);
                    ExitCode::FAILURE
                }
            };
        }

        // Recording requires explicit sources, a duration and an output directory.
        let (Some(seconds), Some(output_dir)) = (options.seconds, options.output_dir.as_ref())
        else {
            eprintln!(
                "encoded_audio_probe: recording needs --source ID=ROUTE, --seconds and --output-dir all explicit; this invocation did not record."
            );
            eprintln!("{USAGE}");
            eprintln!("Currently capturable sources:");
            match list_sources() {
                Ok(text) => eprint!("{text}"),
                Err(error) => eprintln!("  (enumeration failed: {})", error.message),
            }
            return ExitCode::FAILURE;
        };
        if options.sources.is_empty() {
            eprintln!("encoded_audio_probe: recording needs at least one --source ID=ROUTE.");
            eprintln!("{USAGE}");
            return ExitCode::FAILURE;
        }

        let outcome = if options.normalize {
            // The accepted 48 kHz adapter owns the connection; a source it cannot connect
            // fails explicitly instead of reaching the mixer in the wrong format.
            let mut backend = NormalizingBackend::new(PlatformBackend);
            record(
                &mut backend,
                &options.sources,
                seconds,
                output_dir,
                options.normalize,
            )
        } else {
            let mut backend = PlatformBackend;
            record(
                &mut backend,
                &options.sources,
                seconds,
                output_dir,
                options.normalize,
            )
        };
        match outcome {
            Ok(report) => {
                print!("{report}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("encoded_audio_probe failed: {:?}", error.code);
                eprintln!("{}", error.message);
                eprintln!("action: {}", error.action);
                ExitCode::FAILURE
            }
        }
    }

    fn record<B>(
        backend: &mut B,
        sources: &[SelectedRoute],
        seconds: u32,
        output_dir: &Path,
        normalize: bool,
    ) -> Result<String, RecorderError>
    where
        B: AudioCaptureBackend,
        B::Session: DrainableAudioSession,
    {
        // Freeze the accepted plan before anything native starts: an invalid route set
        // fails here, before any file or session exists.
        let routes: Vec<AudioRouteSelection> = sources
            .iter()
            .map(|source| AudioRouteSelection {
                controls: Default::default(),
                muted: false,
                source_id: AudioSourceId(source.source_id.clone()),
                route: source.route,
            })
            .collect();
        let plan = frame_audio::plan_tracks(&routes)?;
        if plan.is_empty() {
            return Err(RecorderError::invalid(
                "The selected routes plan no track; the probe would encode nothing.",
            ));
        }

        std::fs::create_dir_all(output_dir).map_err(|error| output_error(output_dir, &error))?;
        let mut artifacts = Artifacts::open(output_dir, &plan)?;
        let mut evidence = RunEvidence::default();
        let run_started = Instant::now();

        // One clock and one origin for the live pipeline *and* the encoders. The origin is
        // read immediately before start, so it is not later than any source's first native
        // timestamp and no source timestamp has to be shifted to fit it.
        let clock = Arc::new(MasterClock::default());
        let origin = clock.now();
        let pipeline = AudioPipeline::start(backend, &routes, Arc::clone(&clock), origin)?;
        let mut encoded = EncodedAudioPipeline::new(pipeline, origin)?;
        if encoded.origin() != origin {
            return Err(internal(
                "The encoded pipeline was built with a different media origin.".into(),
            ));
        }

        // Live capture: the accepted poll path with a bounded consumer pace, writing every
        // completed packet as it arrives.
        let duration = Duration::from_secs(u64::from(seconds));
        let live_started = Instant::now();
        while live_started.elapsed() < duration {
            evidence.live_polls += 1;
            match encoded.poll(clock.now()) {
                Ok(packets) => artifacts.write_batch(&packets, Phase::Live)?,
                Err(error) => {
                    evidence.failure = Some(error);
                    evidence.failure_phase = Some("live");
                    break;
                }
            }
            std::thread::sleep(POLL_SLEEP);
        }

        // The graceful drain owns the tail: bounded polling, one verdict per step.
        let mut trim: Option<Vec<TrackTrim>> = None;
        if evidence.failure.is_none() {
            match encoded.request_drain() {
                Ok(()) => {
                    evidence.drain_requested = true;
                    let drain_started = Instant::now();
                    loop {
                        if drain_started.elapsed() >= DRAIN_WATCHDOG {
                            evidence.watchdog_exceeded = true;
                            evidence.failure = Some(internal(format!(
                                "The graceful drain did not complete within {} s.",
                                DRAIN_WATCHDOG.as_secs()
                            )));
                            evidence.failure_phase = Some("drain_watchdog");
                            break;
                        }
                        evidence.drain_polls += 1;
                        match encoded.poll_drain() {
                            Ok(EncodedAudioDrainPoll::Packets(packets)) => {
                                artifacts.write_batch(&packets, Phase::Drain)?;
                            }
                            Ok(EncodedAudioDrainPoll::Pending) => {
                                evidence.drain_pending += 1;
                                std::thread::sleep(POLL_SLEEP);
                            }
                            Ok(EncodedAudioDrainPoll::Finished(finish)) => {
                                artifacts.write_batch(&finish.packets, Phase::Finish)?;
                                trim = Some(
                                    finish
                                        .tracks
                                        .iter()
                                        .map(|track| TrackTrim {
                                            pre_skip: track.pre_skip,
                                            discard_padding: track.discard_padding,
                                            input_frames: track.input_frames,
                                        })
                                        .collect(),
                                );
                                evidence.drained = true;
                                break;
                            }
                            // A persistent verdict without an earlier finish report means
                            // the plan had no encoder at all, which the plan check above
                            // already refused.
                            Ok(EncodedAudioDrainPoll::Drained) => {
                                evidence.drained = true;
                                break;
                            }
                            Err(error) => {
                                evidence.failure = Some(error);
                                evidence.failure_phase = Some("drain");
                                break;
                            }
                        }
                    }
                    evidence.drain_ms = drain_started.elapsed().as_millis();
                }
                Err(error) => {
                    evidence.failure = Some(error);
                    evidence.failure_phase = Some("drain_request");
                }
            }
        }

        // Cleanup is explicit on every path: a completed drain keeps its permanent verdict,
        // and an aborted or failed flow closes every native session.
        evidence.stop_result = Some(match encoded.stop() {
            Ok(()) => "ok".to_string(),
            Err(error) => format!("{:?}: {}", error.code, error.message),
        });
        evidence.elapsed_ms = run_started.elapsed().as_millis();

        if let Err(error) = artifacts.flush() {
            retain_failure(&mut evidence.failure, error, "artifact flush");
            evidence.failure_phase = evidence.failure_phase.or(Some("artifact_flush"));
        }

        // The real decoder reads the written packets back from disk.
        let verification = verify(output_dir, &plan, trim.as_deref());
        let complete = evidence.failure.is_none()
            && evidence.drained
            && !evidence.watchdog_exceeded
            && evidence.stop_result.as_deref() == Some("ok")
            && verification.ok();
        let status = if complete { "complete" } else { "failed" };

        let report = Report {
            sources,
            output_dir,
            origin,
            seconds,
            normalize,
            plan: &plan,
            artifacts: &artifacts,
            evidence: &evidence,
            trim: trim.as_deref(),
            verification: &verification,
            status,
        };

        let probe_path = output_dir.join(PROBE_NAME);
        let body = serde_json::to_string_pretty(&report.metadata()).map_err(|error| {
            internal(format!(
                "The probe metadata could not be serialized: {error}"
            ))
        })?;
        let mut probe_file = open_exclusive(&probe_path)?;
        probe_file
            .write_all(body.as_bytes())
            .map_err(|error| output_error(&probe_path, &error))?;
        probe_file
            .write_all(b"\n")
            .map_err(|error| output_error(&probe_path, &error))?;
        probe_file
            .flush()
            .map_err(|error| output_error(&probe_path, &error))?;

        let text = report.text();
        let report_path = output_dir.join(REPORT_NAME);
        let mut report_file = open_exclusive(&report_path)?;
        report_file
            .write_all(text.as_bytes())
            .map_err(|error| output_error(&report_path, &error))?;
        report_file
            .flush()
            .map_err(|error| output_error(&report_path, &error))?;

        if let Some(error) = &evidence.failure {
            return Err(RecorderError {
                code: error.code,
                message: format!(
                    "The encoded audio probe ended with an explicit {} failure. See {}.\n{text}",
                    evidence.failure_phase.unwrap_or("run"),
                    probe_path.display()
                ),
                recovery: RecoveryStatus::NoRecording,
                action: "Read the recorded failure, then re-run the probe without changing the selected source."
                    .to_string(),
            });
        }
        if !verification.ok() {
            return Err(internal(format!(
                "The encoded tracks did not pass the real-decoder verification. See {}.\n{text}",
                probe_path.display()
            )));
        }
        Ok(text)
    }

    /// The whole run's report, as the separated JSON metadata and as operator text.
    struct Report<'a> {
        sources: &'a [SelectedRoute],
        output_dir: &'a Path,
        origin: MediaTimestamp,
        seconds: u32,
        /// FRAME-0206: whether the accepted 48 kHz adapter was inserted before the mixer.
        normalize: bool,
        plan: &'a [TrackPlan],
        artifacts: &'a Artifacts,
        evidence: &'a RunEvidence,
        trim: Option<&'a [TrackTrim]>,
        verification: &'a Verification,
        status: &'a str,
    }

    impl Report<'_> {
        /// Packet counts, origin and PTS, trim metadata, polling, closure and verification
        /// stay separate so no number is restated under another name.
        fn metadata(&self) -> serde_json::Value {
            serde_json::json!({
                "probe": "encoded_audio_probe",
                "status": self.status,
                "complete": self.status == "complete",
                "error": self.evidence.failure.as_ref().map(|error| serde_json::json!({
                    "code": format!("{:?}", error.code),
                    "message": error.message,
                    "action": error.action,
                })),
                "source_routes": self.sources.iter().map(|source| serde_json::json!({
                    "source_id": source.source_id,
                    "route": source.label,
                })).collect::<Vec<_>>(),
                "normalized_48k": self.normalize,
                "output_dir": self.output_dir.display().to_string(),
                "requested_seconds": self.seconds,
                "elapsed_ms": self.evidence.elapsed_ms,
                "origin_ns": self.origin.0,
                "plan": self.plan.iter().enumerate().map(|(track_id, track)| serde_json::json!({
                    "track_id": track_id,
                    "kind": track_label(track),
                    "sources": track.sources.iter().map(|id| id.0.clone()).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
                "packets": {
                    "total": self.artifacts.packets,
                    "bytes": self.artifacts.bytes,
                    "per_track": self.artifacts.tracks.iter().enumerate().map(|(track_id, track)| serde_json::json!({
                        "track_id": track_id,
                        "packets": track.packets,
                        "bytes": track.bytes,
                        "live": track.live,
                        "drain": track.drain,
                        "finish": track.finish,
                    })).collect::<Vec<_>>(),
                },
                "timing": self.artifacts.tracks.iter().enumerate().map(|(track_id, track)| serde_json::json!({
                    "track_id": track_id,
                    "origin_ns": self.origin.0,
                    "first_pts_ns": track.first_pts.map(|pts| pts.0),
                    "last_pts_ns": track.last_pts.map(|pts| pts.0),
                })).collect::<Vec<_>>(),
                "trim": self.trim.map(|tracks| tracks.iter().enumerate().map(|(track_id, track)| serde_json::json!({
                    "track_id": track_id,
                    "pre_skip": track.pre_skip,
                    "discard_padding": track.discard_padding,
                    "input_frames": track.input_frames,
                })).collect::<Vec<_>>()),
                "polling": {
                    "live_polls": self.evidence.live_polls,
                    "drain_polls": self.evidence.drain_polls,
                    "drain_pending_steps": self.evidence.drain_pending,
                },
                "closure": {
                    "drain_requested": self.evidence.drain_requested,
                    "drained": self.evidence.drained,
                    "watchdog_exceeded": self.evidence.watchdog_exceeded,
                    "drain_ms": self.evidence.drain_ms,
                    "stop": self.evidence.stop_result,
                    "failure_phase": self.evidence.failure_phase,
                    "native_cleanup": "A drained verdict is reported only after every capture session was cancelled, stopped, dropped and joined by the owned pipeline.",
                },
                "verification": self.verification,
                "artifacts": {
                    "index": format!("{PACKETS_DIR}/{INDEX_NAME}"),
                    "tracks": (0..self.plan.len())
                        .map(|track_id| format!("{PACKETS_DIR}/track-{track_id}.opus.bin"))
                        .collect::<Vec<_>>(),
                    "report": REPORT_NAME,
                    "metadata": PROBE_NAME,
                },
            })
        }

        fn text(&self) -> String {
            let mut report = format!(
                "encoded_audio_probe: status={} sources={} normalized_48k={} seconds={} origin_ns={}\n",
                self.status,
                self.sources.len(),
                self.normalize,
                self.seconds,
                self.origin.0,
            );
            for (track_id, track) in self.artifacts.tracks.iter().enumerate() {
                let (kind, sources) = match self.plan.get(track_id) {
                    Some(plan) => (
                        track_label(plan),
                        plan.sources
                            .iter()
                            .map(|id| id.0.clone())
                            .collect::<Vec<_>>()
                            .join(","),
                    ),
                    None => ("unknown".to_string(), String::new()),
                };
                report.push_str(&format!(
                    "track {track_id}: kind={kind} sources=[{sources}] packets={} (live={} drain={} finish={}) bytes={} first_pts_ns={:?} last_pts_ns={:?}\n",
                    track.packets,
                    track.live,
                    track.drain,
                    track.finish,
                    track.bytes,
                    track.first_pts.map(|pts| pts.0),
                    track.last_pts.map(|pts| pts.0),
                ));
            }
            if let Some(trim) = self.trim {
                for (track_id, track) in trim.iter().enumerate() {
                    report.push_str(&format!(
                        "trim {track_id}: pre_skip={} discard_padding={} input_frames={}\n",
                        track.pre_skip, track.discard_padding, track.input_frames
                    ));
                }
            }
            report.push_str(&format!(
                "polling: live_polls={} drain_polls={} drain_pending={} drained={} watchdog_exceeded={} drain_ms={}\n",
                self.evidence.live_polls,
                self.evidence.drain_polls,
                self.evidence.drain_pending,
                self.evidence.drained,
                self.evidence.watchdog_exceeded,
                self.evidence.drain_ms,
            ));
            report.push_str(&format!(
                "closure: stop={:?} failure_phase={:?} native_cleanup=every_session_cancelled_stopped_dropped_joined failure={:?}\n",
                self.evidence.stop_result,
                self.evidence.failure_phase,
                self.evidence.failure.as_ref().map(|error| error.message.clone()),
            ));
            report.push_str(&format!(
                "verification: performed={} comparable={} equal_trimmed_frames={:?} content_identical={:?} compared_frames={} mismatched_samples={} max_abs_diff={} failure={:?}\n",
                self.verification.performed,
                self.verification.comparable,
                self.verification.equal_trimmed_frames,
                self.verification.identical,
                self.verification.compared_frames,
                self.verification.mismatched_samples,
                self.verification.max_abs_diff,
                self.verification.failure,
            ));
            for track in &self.verification.tracks {
                report.push_str(&format!(
                    "verify track {}: packets_read={} decoded_frames={} trimmed_frames={} input_frames={} pre_skip={} discard_padding={} trim_invariant={} leading_zero_frames={} error={:?}\n",
                    track.track_id,
                    track.packets_read,
                    track.decoded_frames,
                    track.trimmed_frames,
                    track.input_frames,
                    track.pre_skip,
                    track.discard_padding,
                    track.trim_invariant,
                    track.leading_zero_frames,
                    track.decode_error,
                ));
            }
            report.push_str(&format!(
                "elapsed_ms={}\nartifacts: {}\n",
                self.evidence.elapsed_ms,
                self.output_dir.display()
            ));
            report
        }
    }

    /// One track's real-decoder evidence.
    #[derive(Debug, Serialize)]
    struct TrackVerification {
        track_id: u32,
        packets_read: u64,
        decoded_frames: u64,
        trimmed_frames: u64,
        input_frames: u64,
        pre_skip: u16,
        discard_padding: u16,
        /// `decoded == pre_skip + input_frames + discard_padding` and
        /// `trimmed_frames == input_frames`.
        trim_invariant: bool,
        /// Accepted frames before the first nonzero sample. A leading mixer gap shows up
        /// here; in a single run genuinely silent input is indistinguishable from it.
        leading_zero_frames: u64,
        decode_error: Option<String>,
    }

    /// The whole-run real-decoder verdict, including the cross-track comparison.
    #[derive(Debug, Serialize)]
    struct Verification {
        performed: bool,
        /// True only when every track carries the same source set, so identical content is
        /// the expected result and is therefore required.
        comparable: bool,
        equal_trimmed_frames: Option<bool>,
        identical: Option<bool>,
        compared_frames: u64,
        mismatched_samples: u64,
        first_mismatch_frame: Option<u64>,
        max_abs_diff: f64,
        tracks: Vec<TrackVerification>,
        failure: Option<String>,
    }

    impl Verification {
        fn failed(failure: String) -> Self {
            Self {
                performed: false,
                comparable: false,
                equal_trimmed_frames: None,
                identical: None,
                compared_frames: 0,
                mismatched_samples: 0,
                first_mismatch_frame: None,
                max_abs_diff: 0.0,
                tracks: Vec::new(),
                failure: Some(failure),
            }
        }

        /// The probe's own acceptance rule, stated over real decoder output.
        fn ok(&self) -> bool {
            self.performed
                && self.failure.is_none()
                && self.tracks.iter().all(|track| {
                    track.decode_error.is_none()
                        && track.trim_invariant
                        && track.trimmed_frames == track.input_frames
                })
                && (!self.comparable
                    || (self.equal_trimmed_frames == Some(true) && self.identical == Some(true)))
        }
    }

    /// Reads the streamed JSONL index back, grouped per track, checking that each track's
    /// packet ordinals are the contiguous range the writer produced.
    fn read_index(path: &Path, track_count: usize) -> Result<Vec<Vec<PacketRecord>>, String> {
        let file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
        let mut grouped: Vec<Vec<PacketRecord>> = (0..track_count).map(|_| Vec::new()).collect();
        for (line_number, line) in BufReader::new(file).lines().enumerate() {
            let line = line.map_err(|error| format!("{}: {error}", path.display()))?;
            if line.trim().is_empty() {
                return Err(format!(
                    "{}: blank index line {}",
                    path.display(),
                    line_number + 1
                ));
            }
            let record: PacketRecord = serde_json::from_str(&line).map_err(|error| {
                format!("{}: line {}: {error}", path.display(), line_number + 1)
            })?;
            let track_id = usize::try_from(record.track_id)
                .map_err(|_| format!("packet track id {} is out of range", record.track_id))?;
            let Some(track) = grouped.get_mut(track_id) else {
                return Err(format!(
                    "packet track id {} was never planned",
                    record.track_id
                ));
            };
            if record.index != track.len() as u64 {
                return Err(format!(
                    "track {} packet ordinal {} is not the next one ({})",
                    record.track_id,
                    record.index,
                    track.len()
                ));
            }
            track.push(record);
        }
        Ok(grouped)
    }

    /// The real decoder for one track, with the reported trim applied as it streams.
    struct TrimReader {
        decoder: opus::Decoder,
        pre_skip_remaining: u64,
        limit: u64,
        accepted: u64,
        leading_zero_frames: u64,
        seen_signal: bool,
    }

    impl TrimReader {
        fn new(trim: TrackTrim) -> Result<Self, String> {
            let decoder = opus::Decoder::new(OPUS_RATE, opus::Channels::Stereo)
                .map_err(|error| format!("the Opus decoder could not be created: {error}"))?;
            Ok(Self {
                decoder,
                pre_skip_remaining: u64::from(trim.pre_skip),
                limit: trim.input_frames,
                accepted: 0,
                leading_zero_frames: 0,
                seen_signal: false,
            })
        }

        /// Decodes one packet, drops the head pre-skip and keeps only `input_frames`
        /// accepted frames, appending them to `out`. Returns the decoded frame count.
        fn push(
            &mut self,
            data: &[u8],
            scratch: &mut [f32],
            out: &mut Vec<f32>,
        ) -> Result<usize, String> {
            let frames = self
                .decoder
                .decode_float(data, scratch, false)
                .map_err(|error| format!("a packet did not decode: {error}"))?;
            let mut offset = 0usize;
            if self.pre_skip_remaining > 0 {
                let skip = usize::try_from(self.pre_skip_remaining)
                    .unwrap_or(usize::MAX)
                    .min(frames);
                self.pre_skip_remaining -= skip as u64;
                offset = skip;
            }
            let available = frames - offset;
            let take = usize::try_from(self.limit - self.accepted)
                .unwrap_or(usize::MAX)
                .min(available);
            if take > 0 {
                let start = offset * OPUS_CHANNELS;
                let end = (offset + take) * OPUS_CHANNELS;
                let accepted = &scratch[start..end];
                // Leading all-zero frames are the only visible trace of a leading mixer
                // gap through the accepted API; they are reported, not hidden.
                if !self.seen_signal {
                    for pair in accepted.chunks_exact(OPUS_CHANNELS) {
                        if pair.iter().any(|sample| *sample != 0.0) {
                            self.seen_signal = true;
                            break;
                        }
                        self.leading_zero_frames += 1;
                    }
                }
                out.extend_from_slice(accepted);
                self.accepted += take as u64;
            }
            Ok(frames)
        }
    }

    /// Compares the accepted frames of every track in lockstep, keeping only the bounded
    /// remainder that has not been compared yet.
    #[derive(Default)]
    struct Compare {
        compared_frames: u64,
        mismatched_samples: u64,
        first_mismatch_frame: Option<u64>,
        max_abs_diff: f64,
    }

    impl Compare {
        fn step(&mut self, pending: &mut [Vec<f32>]) {
            let Some(common) = pending.iter().map(Vec::len).min() else {
                return;
            };
            let common = common - common % OPUS_CHANNELS;
            if common == 0 {
                return;
            }
            for sample in 0..common {
                let reference = pending[0][sample];
                for track in pending.iter().skip(1) {
                    let value = track[sample];
                    if value != reference {
                        self.mismatched_samples += 1;
                        if self.first_mismatch_frame.is_none() {
                            self.first_mismatch_frame =
                                Some(self.compared_frames + (sample / OPUS_CHANNELS) as u64);
                        }
                    }
                    let diff = f64::from((value - reference).abs());
                    if diff > self.max_abs_diff {
                        self.max_abs_diff = diff;
                    }
                }
            }
            self.compared_frames += (common / OPUS_CHANNELS) as u64;
            for track in pending.iter_mut() {
                track.drain(..common);
            }
        }
    }

    /// Decodes every written packet with the real decoder and checks the trim claims.
    fn verify(output_dir: &Path, plan: &[TrackPlan], trim: Option<&[TrackTrim]>) -> Verification {
        let Some(trim) = trim else {
            return Verification::failed(
                "The drain never produced its finish report, so no trim metadata exists to verify."
                    .to_string(),
            );
        };
        if trim.len() != plan.len() {
            return Verification::failed(format!(
                "The finish report covers {} tracks but the plan has {}.",
                trim.len(),
                plan.len()
            ));
        }
        let index_path = output_dir.join(PACKETS_DIR).join(INDEX_NAME);
        let grouped = match read_index(&index_path, plan.len()) {
            Ok(grouped) => grouped,
            Err(failure) => return Verification::failed(failure),
        };
        let mut files = Vec::with_capacity(plan.len());
        for track_id in 0..plan.len() {
            let path = output_dir
                .join(PACKETS_DIR)
                .join(format!("track-{track_id}.opus.bin"));
            match File::open(&path) {
                Ok(file) => files.push(file),
                Err(error) => return Verification::failed(format!("{}: {error}", path.display())),
            }
        }

        // Identical content is only expected when every track carries the same sources.
        let comparable =
            plan.len() >= 2 && plan.iter().all(|track| track.sources == plan[0].sources);
        let mut readers = Vec::with_capacity(plan.len());
        let mut tracks = Vec::with_capacity(plan.len());
        for (track_id, track_trim) in trim.iter().enumerate() {
            match TrimReader::new(*track_trim) {
                Ok(reader) => readers.push(reader),
                Err(failure) => return Verification::failed(failure),
            }
            tracks.push(TrackVerification {
                track_id: track_id as u32,
                packets_read: 0,
                decoded_frames: 0,
                trimmed_frames: 0,
                input_frames: track_trim.input_frames,
                pre_skip: track_trim.pre_skip,
                discard_padding: track_trim.discard_padding,
                trim_invariant: false,
                leading_zero_frames: 0,
                decode_error: None,
            });
        }

        let mut scratch = vec![0.0f32; MAX_PACKET_FRAMES * OPUS_CHANNELS];
        let mut payload = Vec::new();
        let mut pending: Vec<Vec<f32>> = vec![Vec::new(); plan.len()];
        let mut compare = Compare::default();
        let longest = grouped.iter().map(Vec::len).max().unwrap_or(0);
        for packet_index in 0..longest {
            for track_id in 0..plan.len() {
                let Some(record) = grouped[track_id].get(packet_index) else {
                    continue;
                };
                if tracks[track_id].decode_error.is_some() {
                    continue;
                }
                let outcome =
                    read_packet(&mut files[track_id], record, &mut payload).and_then(|()| {
                        readers[track_id].push(&payload, &mut scratch, &mut pending[track_id])
                    });
                match outcome {
                    Ok(frames) => {
                        tracks[track_id].packets_read += 1;
                        tracks[track_id].decoded_frames += frames as u64;
                    }
                    Err(failure) => tracks[track_id].decode_error = Some(failure),
                }
            }
            // Every track carries the same quantum count, so a bounded remainder is all
            // that can ever be outstanding. Different sources are never compared.
            if comparable {
                compare.step(&mut pending);
            } else {
                for track in pending.iter_mut() {
                    track.clear();
                }
            }
        }
        for (track_id, reader) in readers.iter().enumerate() {
            let decoded = tracks[track_id].decoded_frames;
            tracks[track_id].trimmed_frames = reader.accepted;
            tracks[track_id].leading_zero_frames = reader.leading_zero_frames;
            // The trim window plus the accepted frames must account for every decoded
            // frame exactly: nothing is dropped silently and nothing is invented.
            tracks[track_id].trim_invariant = tracks[track_id].decode_error.is_none()
                && decoded
                    == u64::from(tracks[track_id].pre_skip)
                        + tracks[track_id].input_frames
                        + u64::from(tracks[track_id].discard_padding)
                && reader.accepted == tracks[track_id].input_frames;
        }

        let equal_trimmed_frames = comparable.then(|| {
            let first = tracks[0].trimmed_frames;
            tracks.iter().all(|track| track.trimmed_frames == first)
        });
        Verification {
            performed: true,
            comparable,
            equal_trimmed_frames,
            identical: comparable
                .then_some(compare.mismatched_samples == 0 && compare.compared_frames > 0),
            compared_frames: compare.compared_frames,
            mismatched_samples: compare.mismatched_samples,
            first_mismatch_frame: compare.first_mismatch_frame,
            max_abs_diff: compare.max_abs_diff,
            tracks,
            failure: None,
        }
    }

    /// Seeks to one packet's offset and reads exactly its recorded size.
    fn read_packet(
        file: &mut File,
        record: &PacketRecord,
        payload: &mut Vec<u8>,
    ) -> Result<(), String> {
        payload.clear();
        payload.resize(record.size as usize, 0);
        file.seek(SeekFrom::Start(record.offset))
            .map_err(|error| format!("seek to {}: {error}", record.offset))?;
        file.read_exact(payload)
            .map_err(|error| format!("read {} bytes at {}: {error}", record.size, record.offset))?;
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn duration_is_explicit_and_bounded() {
            let args = ["--source", "wasapi-mic:test=both", "--output-dir", "unused"];
            let options = parse_options(args.map(str::to_string)).unwrap();
            assert_eq!(options.seconds, None);
            for seconds in ["0", "11"] {
                let args = ["--seconds", seconds];
                assert!(parse_options(args.map(str::to_string)).is_err());
            }
            let args = ["--seconds", "10"];
            assert_eq!(
                parse_options(args.map(str::to_string)).unwrap().seconds,
                Some(10)
            );
        }

        #[test]
        fn a_route_is_explicit_and_ignore_is_refused() {
            let selected = parse_source("wasapi-mic:{a}.{b}=both").unwrap();
            assert_eq!(selected.source_id, "wasapi-mic:{a}.{b}");
            assert_eq!(selected.route, AudioRoute::MainMixAndSeparate);
            assert!(parse_source("wasapi-mic:{a}.{b}=ignore").is_err());
            assert!(parse_source("wasapi-mic:{a}.{b}=loud").is_err());
            assert!(parse_source("wasapi-mic:{a}.{b}").is_err());
            assert!(parse_source("=main").is_err());
        }

        #[test]
        fn later_cleanup_failure_keeps_primary_context() {
            let mut primary = Some(RecorderError {
                code: ErrorCode::DiskFull,
                message: "packet write failed".to_string(),
                recovery: RecoveryStatus::NoRecording,
                action: "Choose another directory".to_string(),
            });
            let cleanup = RecorderError {
                code: ErrorCode::Internal,
                message: "native stop failed".to_string(),
                recovery: RecoveryStatus::NoRecording,
                action: "Inspect device".to_string(),
            };
            retain_failure(&mut primary, cleanup, "artifact flush");
            let retained = primary.unwrap();
            assert_eq!(retained.code, ErrorCode::DiskFull);
            assert!(retained.message.contains("packet write failed"));
            assert!(retained.message.contains("native stop failed"));
        }
    }
}
