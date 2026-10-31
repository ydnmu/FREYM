// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Explicit bounded process-tree PCM evidence. A bare invocation only lists sources.
#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("Process audio requires Windows.");
    std::process::exit(1);
}
#[cfg(target_os = "windows")]
fn main() {
    if let Err(error) = probe::run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
#[cfg(target_os = "windows")]
mod probe {
    use frame_audio::{AudioCancellation, AudioCaptureBackend, AudioContext, AudioSession};
    use frame_clock::MasterClock;
    use frame_contracts::AudioSourceId;
    use frame_windows::PlatformBackend;
    use serde_json::json;
    use std::{
        fs::{File, OpenOptions},
        io::{BufWriter, Write},
        path::{Path, PathBuf},
        sync::Arc,
        time::{Duration, Instant},
    };
    const USAGE: &str = "application_audio_probe --list | --capture --source-id ID --seconds 1..30 --output-dir test-results/NEW";
    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
    fn fresh(path: &Path) -> Result<File> {
        Ok(OpenOptions::new().write(true).create_new(true).open(path)?)
    }
    fn parse(args: &[String]) -> Result<Option<(String, u32, PathBuf)>> {
        if args.is_empty() || args == ["--list"] || args == ["--help"] {
            return Ok(None);
        }
        let (mut capture, mut source, mut seconds, mut output) = (false, None, None, None);
        let mut arguments = args.iter();
        while let Some(flag) = arguments.next() {
            match flag.as_str() {
                "--capture" if !capture => capture = true,
                "--source-id" if source.is_none() => {
                    source = Some(arguments.next().ok_or(USAGE)?.clone())
                }
                "--seconds" if seconds.is_none() => {
                    seconds = Some(arguments.next().ok_or(USAGE)?.parse::<u32>()?)
                }
                "--output-dir" if output.is_none() => {
                    output = Some(PathBuf::from(arguments.next().ok_or(USAGE)?))
                }
                _ => return Err(USAGE.into()),
            }
        }
        let (Some(source), Some(seconds), Some(output)) = (source, seconds, output) else {
            return Err(USAGE.into());
        };
        if !capture || !(1..=30).contains(&seconds) || !source.starts_with("wasapi-session:") {
            return Err(USAGE.into());
        }
        Ok(Some((source, seconds, output)))
    }
    fn output_directory(path: &Path) -> Result<PathBuf> {
        let ignored = std::fs::canonicalize("test-results")?;
        let parent = std::fs::canonicalize(path.parent().ok_or("Output needs a parent")?)?;
        if !parent.starts_with(&ignored) {
            return Err("Output must be inside the existing ignored test-results directory".into());
        }
        let directory = parent.join(
            path.file_name()
                .ok_or("Output needs a new directory name")?,
        );
        std::fs::create_dir(&directory)?;
        Ok(directory)
    }
    pub(super) fn run() -> Result<()> {
        let args: Vec<_> = std::env::args().skip(1).collect();
        let Some((id, seconds, directory)) = parse(&args)? else {
            if args == ["--help"] {
                println!("{USAGE}");
                return Ok(());
            }
            let sources =
                frame_windows::enumerate_application_audio_sources().map_err(|e| e.message)?;
            println!("{}", serde_json::to_string_pretty(&sources)?);
            return Ok(());
        };
        let directory = output_directory(&directory)?;
        let mut pcm =
            BufWriter::with_capacity(256 * 1024, fresh(&directory.join("capture.f32le"))?);
        let mut packets =
            BufWriter::with_capacity(256 * 1024, fresh(&directory.join("packets.jsonl"))?);
        let mut report = fresh(&directory.join("capture.json"))?;
        let source = AudioSourceId(id.clone());
        let context = AudioContext {
            clock: Arc::new(MasterClock::default()),
            cancellation: AudioCancellation::default(),
        };
        let started = PlatformBackend.start_audio(&source, context);
        let mut session = match started {
            Ok(session) => session,
            Err(e) => {
                serde_json::to_writer_pretty(
                    &mut report,
                    &json!({"complete":false,"cleanupJoined":true,"sourceId":id,"errorCode":format!("{:?}",e.code),"error":e.message}),
                )?;
                return Err(e.message.into());
            }
        };
        let begin = Instant::now();
        let deadline = begin + Duration::from_secs(u64::from(seconds));
        let mut blocks = 0u64;
        let mut frames = 0u64;
        let mut error_code = None;
        let capture: Result<()> = (|| {
            while Instant::now() < deadline {
                let block = match session.try_pcm() {
                    Ok(value) => value,
                    Err(e) => {
                        error_code = Some(format!("{:?}", e.code));
                        return Err(e.message.into());
                    }
                };
                let Some(block) = block else {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                };
                if block.source_id != source
                    || block.sample_rate.get() != 48_000
                    || block.channels.get() != 2
                    || block.samples.len() % 2 != 0
                {
                    return Err("Capture returned an unexpected source/format/alignment".into());
                }
                if blocks >= 30_000 || frames + block.samples.len() as u64 / 2 > 31 * 48_000 {
                    return Err("Probe metadata/PCM bound exceeded".into());
                }
                let mut bytes = Vec::with_capacity(block.samples.len() * 4);
                for sample in &block.samples {
                    bytes.extend_from_slice(&sample.to_le_bytes());
                }
                pcm.write_all(&bytes)?;
                serde_json::to_writer(
                    &mut packets,
                    &json!({"index":blocks,"timestampNs":block.timestamp.0,"frames":block.samples.len()/2,"sampleOffset":frames*2}),
                )?;
                packets.write_all(b"\n")?;
                frames += block.samples.len() as u64 / 2;
                blocks += 1;
            }
            if frames == 0 {
                return Err("No PCM was delivered; silence is not a capture proof".into());
            }
            Ok(())
        })();
        let mut failure = capture.err().map(|e| e.to_string());
        if let Err(e) = session.stop() {
            error_code.get_or_insert_with(|| format!("{:?}", e.code));
            let previous = failure.take().unwrap_or_default();
            failure = Some(format!("{previous} Stop: {}", e.message));
        }
        let diagnostics = session.diagnostics().map_err(|e| e.message)?;
        // The control thread has joined before any output/serialization error can return.
        drop(session);
        for result in [pcm.flush(), packets.flush()] {
            if let Err(e) = result {
                failure = Some(format!("{} Output flush: {e}", failure.unwrap_or_default()));
            }
        }
        serde_json::to_writer_pretty(
            &mut report,
            &json!({"task":"FRAME-0106B","sourceId":id,
            "complete":failure.is_none(),"cleanupJoined":true,"processTree":true,"sampleRate":48000,"channels":2,
            "sampleFormat":"f32le","requestedSeconds":seconds,"elapsedSeconds":begin.elapsed().as_secs_f64(),
            "blocks":blocks,"frames":frames,"pcmBytes":frames*8,"errorCode":error_code,"error":failure,"diagnostics":diagnostics}),
        )?;
        report.flush()?;
        if let Some(failure) = failure {
            return Err(failure.into());
        }
        println!("{} frames in {} blocks; stopped and joined", frames, blocks);
        Ok(())
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn capture_is_explicit_bounded_and_application_only() {
            assert!(parse(&[]).unwrap().is_none());
            for args in [
                vec!["--capture"],
                vec![
                    "--capture",
                    "--source-id",
                    "wasapi-mic:x",
                    "--seconds",
                    "8",
                    "--output-dir",
                    "test-results/new",
                ],
                vec![
                    "--source-id",
                    "wasapi-session:x",
                    "--seconds",
                    "8",
                    "--output-dir",
                    "test-results/new",
                ],
                vec![
                    "--capture",
                    "--source-id",
                    "wasapi-session:x",
                    "--seconds",
                    "31",
                    "--output-dir",
                    "test-results/new",
                ],
            ] {
                assert!(parse(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()).is_err());
            }
        }
    }
}
