// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Recovers the complete prefix of an unfinished Matroska recording.
//!
//! The source file is opened read-only and is never modified. The output path is
//! created exclusively: an existing file is refused instead of overwritten. The
//! recovered container carries a new cluster stream, cue index, seek head and
//! duration, while every recovered block payload, its variable frame timestamp, its
//! `DiscardPadding` and the whole `Tracks` element are copied as they were found.
//!
//! ```text
//! cargo run --locked -p frame-mux --example recover_mkv -- \
//!     --source <unfinished.mkv> --output <recovered.mkv>
//! ```
//!
//! The printed JSON reports recovered and dropped-incomplete bytes and packets, the
//! covered prefix, the unparsed tail and whether the encoder's final audio trim is
//! known. A successful recovery does not mean the original recording finished.

use frame_mux::recover;
use serde_json::{Value, json};
use std::path::PathBuf;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const USAGE: &str = "recover_mkv --source <unfinished.mkv> --output <recovered.mkv>";

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|arg| arg == "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    let mut source = None;
    let mut output = None;
    let mut index = 0;
    while index < args.len() {
        let name = args[index].as_str();
        match name {
            "--source" => source = Some(PathBuf::from(take_value(&args, &mut index, name)?)),
            "--output" => output = Some(PathBuf::from(take_value(&args, &mut index, name)?)),
            other => return Err(format!("unknown argument {other}\n{USAGE}").into()),
        }
        index += 1;
    }
    let source = source.ok_or_else(|| format!("--source is required\n{USAGE}"))?;
    let output = output.ok_or_else(|| format!("--output is required\n{USAGE}"))?;

    let summary = recover(&source, &output)?;
    let report = json!({
        "source": summary.source,
        "output": summary.output,
        "source_bytes": summary.source_bytes,
        "output_bytes": summary.output_bytes,
        "recovered_bytes": summary.recovered_bytes,
        "dropped_incomplete_bytes": summary.dropped_incomplete_bytes,
        "dropped_incomplete_packets": summary.dropped_incomplete_packets,
        "unknown_tail": {
            "start_offset": summary.unknown_tail.start_offset,
            "bytes": summary.unknown_tail.bytes,
            "kind": summary.unknown_tail.kind.as_str(),
            "reason": summary.unknown_tail.reason,
        },
        "recovered_packets": summary.recovered_packets,
        "recovered_video_packets": summary.recovered_video_packets,
        "recovered_audio_packets": summary.recovered_audio_packets,
        "recovered_keyframes": summary.recovered_keyframes,
        "cues": summary.cues,
        "clusters": summary.clusters,
        "timestamp_scale_ns": summary.timestamp_scale_ns,
        "container_end_ns": summary.container_end_ns,
        "duration_ns": summary.duration_ns,
        "source_declared_duration_ns": summary.source_declared_duration_ns,
        "source_finalized": summary.source_finalized,
        "final_trim_known": summary.final_trim_known,
        "complete_recording": false,
        "tracks": summary.tracks.iter().map(|track| json!({
            "number": track.number,
            "kind": track.kind.as_str(),
            "codec_id": track.codec_id,
            "name": track.name,
            "codec_delay_ns": track.codec_delay_ns,
            "seek_pre_roll_ns": track.seek_pre_roll_ns,
            "codec_private_bytes": track.codec_private_bytes,
            "opus_pre_skip_samples": track.opus_pre_skip_samples,
            "packets": track.packets,
            "keyframes": track.keyframes,
            "last_end_ns": track.last_end_ns,
            "last_discard_padding_ns": track.last_discard_padding_ns,
        })).collect::<Vec<Value>>(),
        "skipped_elements": summary.skipped_elements.iter().map(|element| json!({
            "name": element.name,
            "id": format!("0x{:X}", element.id),
            "bytes": element.bytes,
            "count": element.count,
        })).collect::<Vec<Value>>(),
        "notes": summary.notes,
        "working_bytes_estimate": summary.working_bytes_estimate,
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn take_value(args: &[String], index: &mut usize, name: &str) -> Result<String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{name} needs a value\n{USAGE}").into())
}
