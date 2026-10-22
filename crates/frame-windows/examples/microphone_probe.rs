// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Diagnostic Windows microphone PCM proof probe (FRAME-0105).
//!
//! Records one explicitly selected capture endpoint as interleaved `f32le` PCM plus a
//! `format`/`timestamp`/`count` metadata file, both created exclusively in an ignored
//! output directory. This is a proof of the WASAPI microphone capture path: it is not a
//! recorder product, it never overwrites evidence, and it never changes a device default,
//! a privacy setting or a system volume.
//!
//! Recording happens ONLY when `--source-id`, `--seconds` and `--output-dir` are all given
//! explicitly. `--list`, a bare invocation and any partial invocation never record.
//!
//! ```text
//! cargo run --locked -p frame-windows --example microphone_probe -- \
//!     --source-id "wasapi-mic:{0.0.0.00000000}.{...}" \
//!     --seconds 30 --output-dir test-results/frame-0105-final/proof-30s
//! ```
//!
//! Options: `--list` (enumerate endpoints, never records), `--source-id ID` (the full
//! `wasapi-mic:...` identity from `--list`), `--seconds N` (1..=300), `--output-dir DIR`,
//! and `--stall-consumer-ms N` (hold each dequeue N ms to build bounded-queue pressure;
//! a queue-pressure run is expected to fail the source explicitly, not record).

#[cfg(target_os = "windows")]
fn main() -> std::process::ExitCode {
    probe::run()
}

#[cfg(not(target_os = "windows"))]
fn main() -> std::process::ExitCode {
    eprintln!("Native microphone capture is implemented for Windows only.");
    std::process::ExitCode::FAILURE
}

#[cfg(target_os = "windows")]
mod probe {
    use frame_audio::{
        AudioCancellation, AudioCaptureBackend, AudioContext, AudioSession, PcmBlock,
    };
    use frame_clock::MasterClock;
    use frame_contracts::{AudioSourceId, ErrorCode, RecorderError, RecoveryStatus};
    use frame_windows::PlatformBackend;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::ExitCode;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const USAGE: &str = "usage: microphone_probe --list | --source-id ID --seconds N --output-dir DIR [--stall-consumer-ms N]";
    const MAX_SECONDS: u32 = 300;
    const MAX_BLOCK_METADATA: usize = 60_000;
    const PCM_FILE: &str = "capture.f32le";
    const META_FILE: &str = "capture.meta.json";

    struct Options {
        list: bool,
        source_id: Option<String>,
        seconds: Option<u32>,
        output_dir: Option<PathBuf>,
        stall_consumer_ms: u32,
    }

    fn parse_options(arguments: impl IntoIterator<Item = String>) -> Result<Options, String> {
        let mut options = Options {
            list: false,
            source_id: None,
            seconds: None,
            output_dir: None,
            stall_consumer_ms: 0,
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
                "--source-id" => options.source_id = Some(take("--source-id")?),
                "--seconds" => {
                    let raw = take("--seconds")?;
                    let seconds: u32 = raw
                        .parse()
                        .map_err(|_| format!("--seconds expects a number, not {raw}"))?;
                    if !(1..=MAX_SECONDS).contains(&seconds) {
                        return Err(format!(
                            "--seconds must be 1..={MAX_SECONDS}, not {seconds}"
                        ));
                    }
                    options.seconds = Some(seconds);
                }
                "--output-dir" => options.output_dir = Some(PathBuf::from(take("--output-dir")?)),
                "--stall-consumer-ms" => {
                    let raw = take("--stall-consumer-ms")?;
                    options.stall_consumer_ms = raw
                        .parse()
                        .map_err(|_| format!("--stall-consumer-ms expects a number, not {raw}"))?;
                }
                "--help" | "-h" => {
                    println!("{USAGE}");
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument {other}")),
            }
        }
        Ok(options)
    }

    /// Lists the enumerated capture endpoints so an operator copies a real identity.
    fn list_sources() -> Result<String, RecorderError> {
        use frame_audio::AudioBackend;
        let sources: Vec<_> = PlatformBackend
            .enumerate_audio_sources()?
            .into_iter()
            .filter(|source| source.kind == frame_contracts::AudioSourceKind::Microphone)
            .collect();
        let mut text = format!("microphone sources: {}\n", sources.len());
        for source in &sources {
            text.push_str(&format!("  {} — {:?}\n", source.id.0, source.display_name));
        }
        if sources.is_empty() {
            text.push_str(
                "  (no active capture endpoints were reported; this is an honest empty result, not a fallback)\n",
            );
        }
        Ok(text)
    }

    fn output_error(path: &Path, error: &std::io::Error) -> RecorderError {
        let code = if error.kind() == std::io::ErrorKind::StorageFull {
            ErrorCode::DiskFull
        } else {
            ErrorCode::InvalidOutputPath
        };
        let detail = if error.kind() == std::io::ErrorKind::AlreadyExists {
            "already exists, and this proof never overwrites evidence"
        } else {
            "could not be written"
        };
        RecorderError {
            code,
            message: format!("The proof artifact {} {detail}: {error}.", path.display()),
            recovery: RecoveryStatus::NoRecording,
            action: "Choose a fresh output directory under the ignored test-results/ tree."
                .to_string(),
        }
    }

    fn retain_failure(primary: &mut Option<RecorderError>, later: RecorderError, phase: &str) {
        if let Some(primary) = primary {
            if primary.message != later.message {
                primary
                    .message
                    .push_str(&format!(" {phase} also failed: {}", later.message));
            }
        } else {
            *primary = Some(later);
        }
    }

    /// Opens one path exclusively, refusing to overwrite existing evidence.
    fn open_exclusive(path: &Path) -> Result<std::fs::File, RecorderError> {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| output_error(path, &error))
    }

    /// Writes one block's interleaved samples as `f32le`, returning the byte count.
    fn write_f32le(writer: &mut impl Write, samples: &[f32]) -> Result<u64, RecorderError> {
        let mut bytes = Vec::with_capacity(samples.len() * 4);
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        let written = bytes.len() as u64;
        writer
            .write_all(&bytes)
            .map_err(|error| output_error(Path::new(PCM_FILE), &error))?;
        Ok(written)
    }

    pub(super) fn run() -> ExitCode {
        let options = match parse_options(std::env::args().skip(1)) {
            Ok(options) => options,
            Err(message) => {
                eprintln!("microphone_probe: {message}");
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
                    eprintln!("microphone_probe failed: {:?}", error.code);
                    eprintln!("{}", error.message);
                    eprintln!("action: {}", error.action);
                    ExitCode::FAILURE
                }
            };
        }

        // Recording requires an explicit source, duration and output directory. Anything
        // else is a non-recording invocation: it lists endpoints and refuses to capture.
        let (Some(source_id), Some(_seconds), Some(output_dir)) = (
            options.source_id.as_ref(),
            options.seconds,
            options.output_dir.as_ref(),
        ) else {
            eprintln!(
                "microphone_probe: recording needs --source-id, --seconds and --output-dir all explicit; this invocation did not record."
            );
            eprintln!("{USAGE}");
            eprintln!("Currently enumerated capture endpoints:");
            match list_sources() {
                Ok(text) => eprint!("{text}"),
                Err(error) => eprintln!("  (enumeration failed: {})", error.message),
            }
            return ExitCode::FAILURE;
        };

        match record(source_id, output_dir, &options) {
            Ok(report) => {
                print!("{report}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("microphone_probe failed: {:?}", error.code);
                eprintln!("{}", error.message);
                eprintln!("action: {}", error.action);
                ExitCode::FAILURE
            }
        }
    }

    fn record(
        source_id: &str,
        output_dir: &Path,
        options: &Options,
    ) -> Result<String, RecorderError> {
        std::fs::create_dir_all(output_dir).map_err(|error| output_error(output_dir, &error))?;
        let pcm_path = output_dir.join(PCM_FILE);
        let meta_path = output_dir.join(META_FILE);
        let pcm_file = open_exclusive(&pcm_path)?;
        let mut writer = std::io::BufWriter::new(pcm_file);

        let clock = Arc::new(MasterClock::default());
        let context = AudioContext {
            clock: Arc::clone(&clock),
            cancellation: AudioCancellation::default(),
        };
        let source = AudioSourceId(source_id.to_string());
        let mut backend = PlatformBackend;
        let mut session = backend.start_audio(&source, context)?;

        let seconds = options
            .seconds
            .expect("recording requires an explicit duration");
        let started = Instant::now();
        let mut blocks: Vec<serde_json::Value> = Vec::new();
        let mut total_frames: u64 = 0;
        let mut total_bytes: u64 = 0;
        let mut format: Option<(u32, u16)> = None;
        let mut run_error: Option<RecorderError> = None;
        loop {
            if started.elapsed() >= Duration::from_secs(u64::from(seconds)) {
                break;
            }
            match session.try_pcm() {
                Ok(Some(block)) => {
                    let PcmBlock {
                        timestamp,
                        sample_rate,
                        channels,
                        samples,
                        ..
                    } = block;
                    let rate = sample_rate.get();
                    let channel_count = channels.get();
                    format.get_or_insert((rate, channel_count));
                    let frames = (samples.len() / usize::from(channel_count)) as u64;
                    let byte_offset = total_bytes;
                    if blocks.len() >= MAX_BLOCK_METADATA {
                        run_error = Some(RecorderError {
                            code: ErrorCode::Internal,
                            message: format!(
                                "The proof reached its {MAX_BLOCK_METADATA}-block metadata limit."
                            ),
                            recovery: RecoveryStatus::NoRecording,
                            action: "Use a shorter diagnostic run.".to_string(),
                        });
                        break;
                    }
                    let written = match write_f32le(&mut writer, &samples) {
                        Ok(written) => written,
                        Err(error) => {
                            run_error = Some(error);
                            break;
                        }
                    };
                    blocks.push(serde_json::json!({
                        "timestamp_ns": timestamp.0,
                        "frames": frames,
                        "byte_offset": byte_offset,
                    }));
                    total_frames += frames;
                    total_bytes += written;
                }
                Ok(None) => {}
                Err(error) => {
                    run_error = Some(error);
                    break;
                }
            }
            // A stalled consumer builds bounded-queue pressure; a zero stall still yields
            // so the producer thread can fill the queue between polls.
            let stall = if options.stall_consumer_ms > 0 {
                options.stall_consumer_ms
            } else {
                1
            };
            std::thread::sleep(Duration::from_millis(u64::from(stall)));
        }

        // Every path after start explicitly stops the native session. Keep an earlier
        // write or capture failure when flushing or cleanup also fails.
        if let Err(error) = writer
            .flush()
            .map_err(|error| output_error(&pcm_path, &error))
        {
            retain_failure(&mut run_error, error, "PCM flush");
        }
        if let Err(error) = session.stop() {
            retain_failure(&mut run_error, error, "session stop");
        }
        let diagnostics = match session.diagnostics() {
            Ok(diagnostics) => diagnostics,
            Err(error) => {
                let message = error.message.clone();
                retain_failure(&mut run_error, error, "diagnostics");
                message
            }
        };

        let primary = run_error;
        let complete = primary.is_none();
        let (status, error_text) = match &primary {
            Some(error) => ("failed", Some(error.message.clone())),
            None => ("complete", None),
        };
        let (sample_rate, channels) = format.unwrap_or((0, 0));
        let meta = serde_json::json!({
            "source_id": source_id,
            "pcm_file": PCM_FILE,
            "format": {
                "encoding": "f32le",
                "sample_rate": sample_rate,
                "channels": channels,
                "bytes_per_sample": 4,
            },
            "status": status,
            "complete": complete,
            "error": error_text,
            "requested_seconds": seconds,
            "stall_consumer_ms": options.stall_consumer_ms,
            "totals": {
                "blocks": blocks.len(),
                "frames": total_frames,
                "bytes": total_bytes,
            },
            "blocks": blocks,
            "diagnostics": diagnostics,
        });
        let mut meta_file = open_exclusive(&meta_path)?;
        let body = serde_json::to_string_pretty(&meta).map_err(|error| RecorderError {
            code: ErrorCode::Internal,
            message: format!("The proof metadata could not be serialized: {error}"),
            recovery: RecoveryStatus::NoRecording,
            action: "This is a defect in the proof metadata schema.".to_string(),
        })?;
        meta_file
            .write_all(body.as_bytes())
            .map_err(|error| output_error(&meta_path, &error))?;
        meta_file
            .write_all(b"\n")
            .map_err(|error| output_error(&meta_path, &error))?;
        meta_file
            .flush()
            .map_err(|error| output_error(&meta_path, &error))?;

        let mut report = format!(
            "microphone_proof: source={source_id} status={status} blocks={} frames={total_frames} bytes={total_bytes}\n",
            blocks.len()
        );
        report.push_str(&format!(
            "format: encoding=f32le sample_rate={sample_rate} channels={channels}\n"
        ));
        report.push_str(&format!("pcm: {}\n", pcm_path.display()));
        report.push_str(&format!("meta: {}\n", meta_path.display()));
        if let Some(error) = &primary {
            report.push_str(&format!("failure: {}\n", error.message));
        }
        report.push_str("capture diagnostics:\n");
        for line in diagnostics.lines().take(40) {
            report.push_str(&format!("  {line}\n"));
        }
        if primary.is_some() {
            // A queue-pressure or other source failure is expected evidence, but the run
            // is still reported as failed rather than a successful recording.
            return Err(RecorderError {
                code: primary.as_ref().map_or(ErrorCode::Internal, |error| error.code),
                message: format!(
                    "The microphone proof ended with an explicit source failure after {} block(s). See {} for the recorded failure.\n{report}",
                    blocks.len(),
                    meta_path.display()
                ),
                recovery: RecoveryStatus::NoRecording,
                action: "A queue-pressure failure is expected evidence; re-run without --stall-consumer-ms for a clean recording."
                    .to_string(),
            });
        }
        Ok(report)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn duration_is_explicit_and_bounded() {
            let args = ["--source-id", "wasapi-mic:test", "--output-dir", "unused"];
            let options = parse_options(args.map(str::to_string)).unwrap();
            assert_eq!(options.seconds, None);

            for seconds in ["0", "301"] {
                let args = ["--seconds", seconds];
                assert!(parse_options(args.map(str::to_string)).is_err());
            }
            let args = ["--seconds", "300"];
            assert_eq!(
                parse_options(args.map(str::to_string)).unwrap().seconds,
                Some(300)
            );
        }

        #[test]
        fn later_cleanup_failure_keeps_primary_context() {
            let mut primary = Some(RecorderError {
                code: ErrorCode::DiskFull,
                message: "PCM write failed".to_string(),
                recovery: RecoveryStatus::NoRecording,
                action: "Choose another directory".to_string(),
            });
            let cleanup = RecorderError {
                code: ErrorCode::Internal,
                message: "native stop failed".to_string(),
                recovery: RecoveryStatus::NoRecording,
                action: "Inspect device".to_string(),
            };
            retain_failure(&mut primary, cleanup, "session stop");
            let retained = primary.unwrap();
            assert_eq!(retained.code, ErrorCode::DiskFull);
            assert!(retained.message.contains("PCM write failed"));
            assert!(retained.message.contains("native stop failed"));
        }
    }
}
