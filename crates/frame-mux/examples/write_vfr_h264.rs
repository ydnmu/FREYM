// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Writes one real encoder proof stream into Matroska with its own variable frame
//! timestamps.
//!
//! The input `.h264` file and its `.jsonl` sidecar are opened read-only and consumed
//! packet by packet: the sidecar supplies each packet's byte range, PTS, DTS,
//! duration and keyframe flag exactly as the hardware encoder reported them, and the
//! elementary stream supplies the bytes. No frame is decoded, re-encoded or retimed,
//! and no nominal frame rate is applied to the timeline.
//!
//! ```text
//! cargo run --locked -p frame-mux --example write_vfr_h264 -- \
//!     --input  <stream.h264> --output <new.mkv> [--sidecar <stream.h264.jsonl>] \
//!     [--origin-ns <ns>]
//! ```
//!
//! The output path is created exclusively, so an existing recording is never
//! overwritten. `--origin-ns` defaults to the first packet's PTS, which is the
//! caller's explicit choice of the shared session origin for this single-track file.

use frame_clock::MediaTimestamp;
use frame_encode::{BitstreamFormat, EncodedPacket, EncodedStreamInfo, EncoderCapability};
use frame_mux::{ContainerWriter, MkvVideoWriter};
use serde_json::{Value, json};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const USAGE: &str = "write_vfr_h264 --input <stream.h264> --output <new.mkv> [--sidecar <stream.h264.jsonl>] [--origin-ns <ns>]";

/// One packet exactly as the encoder's sidecar recorded it.
struct RecordedPacket {
    offset: u64,
    bytes: usize,
    pts_ns: u64,
    dts_ns: u64,
    duration_ns: u64,
    keyframe: bool,
}

struct Sidecar {
    settings: Value,
    packets: Vec<RecordedPacket>,
    summary: Option<Value>,
}

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
    let mut input = None;
    let mut output = None;
    let mut sidecar_path = None;
    let mut origin_ns = None;
    let mut index = 0;
    while index < args.len() {
        let name = args[index].as_str();
        match name {
            "--input" => input = Some(PathBuf::from(take_value(&args, &mut index, name)?)),
            "--output" => output = Some(PathBuf::from(take_value(&args, &mut index, name)?)),
            "--sidecar" => sidecar_path = Some(PathBuf::from(take_value(&args, &mut index, name)?)),
            "--origin-ns" => {
                origin_ns = Some(take_value(&args, &mut index, name)?.parse::<u64>()?);
            }
            other => return Err(format!("unknown argument {other}\n{USAGE}").into()),
        }
        index += 1;
    }
    let input = input.ok_or_else(|| format!("--input is required\n{USAGE}"))?;
    let output = output.ok_or_else(|| format!("--output is required\n{USAGE}"))?;
    let sidecar_path = sidecar_path.unwrap_or_else(|| {
        let mut name = input.clone().into_os_string();
        name.push(".jsonl");
        PathBuf::from(name)
    });

    let sidecar = read_sidecar(&sidecar_path)?;
    let stream_bytes = std::fs::metadata(&input)?.len();
    let mut expected_offset = 0;
    for packet in &sidecar.packets {
        if packet.offset != expected_offset
            || packet.bytes == 0
            || packet.bytes > frame_mux::MAX_PACKET_BYTES
        {
            return Err("invalid, noncontiguous or oversized sidecar packet".into());
        }
        let end = packet
            .offset
            .checked_add(packet.bytes as u64)
            .ok_or("a sidecar packet range overflows")?;
        expected_offset = end;
        if end > stream_bytes {
            return Err(format!(
                "the sidecar describes bytes {}..{end} but {} holds {stream_bytes} bytes",
                packet.offset,
                input.display()
            )
            .into());
        }
        if packet.dts_ns != packet.pts_ns {
            return Err(format!(
                "the packet at offset {} has DTS {} and PTS {}; this writer stores DTS equal to PTS",
                packet.offset, packet.dts_ns, packet.pts_ns
            )
            .into());
        }
    }
    if expected_offset != stream_bytes {
        return Err("sidecar does not cover the full input".into());
    }
    if let Some(summary) = &sidecar.summary
        && let Some(written) = summary.get("packets_written").and_then(Value::as_u64)
        && written != sidecar.packets.len() as u64
    {
        return Err(format!(
            "the sidecar summary reports {written} written packets but lists {}",
            sidecar.packets.len()
        )
        .into());
    }

    let mut file = BufReader::new(File::open(&input)?);
    let first = sidecar
        .packets
        .first()
        .ok_or("the sidecar lists no packets")?;
    let origin_ns = origin_ns.unwrap_or(first.pts_ns);

    // The sidecar records the codec configuration's size, not its bytes, so the
    // parameter sets are recovered from the access unit the encoder emitted first and
    // checked against the recorded size.
    let first_data = read_packet(&mut file, first)?;
    let codec_config = leading_parameter_sets(&first_data)?;
    if let Some(recorded) = sidecar
        .settings
        .get("codec_config_bytes")
        .and_then(Value::as_u64)
        && recorded != codec_config.len() as u64
    {
        return Err(format!(
            "the first access unit carries {} codec configuration bytes but the sidecar recorded {recorded}",
            codec_config.len()
        )
        .into());
    }

    let stream = stream_info(&sidecar.settings, codec_config)?;
    let mut writer = MkvVideoWriter::create(&output, &stream, MediaTimestamp(origin_ns))?;

    for (position, packet) in sidecar.packets.iter().enumerate() {
        let data = if position == 0 {
            first_data.clone()
        } else {
            read_packet(&mut file, packet)?
        };
        writer.write(EncodedPacket {
            // The single video track the hardware encoder emits.
            track_id: 0,
            pts: MediaTimestamp(packet.pts_ns),
            dts: MediaTimestamp(packet.dts_ns),
            duration_ns: packet.duration_ns,
            keyframe: packet.keyframe,
            data,
        })?;
    }

    let stats = writer.stats();
    let artifact = writer.finalize()?;
    let report = json!({
        "artifact": artifact,
        "origin_ns": origin_ns,
        "input": input.to_string_lossy(),
        "sidecar": sidecar_path.to_string_lossy(),
        "container_packets": stats.packets,
        "container_keyframes": stats.keyframes,
        "raw_payload_bytes": stats.raw_payload_bytes,
        "longest_packet_bytes": stats.longest_packet_bytes,
        "first_container_pts_ns": stats.first_pts_ns,
        "last_container_end_ns": stats.last_end_ns,
        "retained_bytes_estimate": stats.retained_bytes_estimate,
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

fn read_sidecar(path: &Path) -> Result<Sidecar> {
    if std::fs::metadata(path)?.len() > 8 * 1024 * 1024 {
        return Err("sidecar exceeds the 8 MiB proof limit".into());
    }
    let text = std::fs::read_to_string(path)?;
    let mut settings = None;
    let mut packets = Vec::new();
    let mut summary = None;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let row: Value = serde_json::from_str(line)?;
        match row.get("kind").and_then(Value::as_str) {
            Some("settings") => settings = Some(row),
            Some("packet") => packets.push(RecordedPacket {
                offset: number(&row, "offset")?,
                bytes: usize::try_from(number(&row, "bytes")?)?,
                pts_ns: number(&row, "pts_ns")?,
                dts_ns: number(&row, "dts_ns")?,
                duration_ns: number(&row, "duration_ns")?,
                keyframe: row
                    .get("keyframe")
                    .and_then(Value::as_bool)
                    .ok_or("a packet record has no keyframe flag")?,
            }),
            Some("summary") => summary = Some(row),
            _ => {}
        }
    }
    Ok(Sidecar {
        settings: settings.ok_or("the sidecar has no settings record")?,
        packets,
        summary,
    })
}

fn number(row: &Value, field: &str) -> Result<u64> {
    row.get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("a sidecar record has no integer {field}").into())
}

fn read_packet(file: &mut BufReader<File>, packet: &RecordedPacket) -> Result<Vec<u8>> {
    let mut data = vec![0u8; packet.bytes];
    file.seek(SeekFrom::Start(packet.offset))?;
    file.read_exact(&mut data)?;
    Ok(data)
}

/// Collects the leading sequence and picture parameter sets of an access unit, which
/// is the codec configuration the encoder repeated in stream.
fn leading_parameter_sets(data: &[u8]) -> Result<Vec<u8>> {
    let mut config = Vec::new();
    let mut cursor = 0usize;
    while let Some((start, end)) = next_nal(data, cursor) {
        let nal_type = data[start] & 0x1F;
        if nal_type != 7 && nal_type != 8 {
            break;
        }
        config.extend_from_slice(&[0, 0, 0, 1]);
        config.extend_from_slice(&data[start..end]);
        cursor = end;
    }
    if config.is_empty() {
        return Err("the first access unit does not begin with parameter sets".into());
    }
    Ok(config)
}

/// Returns the payload range of the NAL unit starting at `from`, if one does.
fn next_nal(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let start = if data.len() >= from + 4 && data[from..from + 4] == [0, 0, 0, 1] {
        from + 4
    } else if data.len() >= from + 3 && data[from..from + 3] == [0, 0, 1] {
        from + 3
    } else {
        return None;
    };
    if start >= data.len() {
        return None;
    }
    let mut end = data.len();
    let mut index = start;
    while index + 3 <= data.len() {
        if data[index..index + 3] == [0, 0, 1] {
            end = if index > start && data[index - 1] == 0 {
                index - 1
            } else {
                index
            };
            break;
        }
        index += 1;
    }
    Some((start, end))
}

fn stream_info(settings: &Value, codec_config: Vec<u8>) -> Result<EncodedStreamInfo> {
    let width = u32::try_from(number(settings, "width")?)?;
    let height = u32::try_from(number(settings, "height")?)?;
    let rate_num = u32::try_from(number(settings, "nominal_rate_num")?)?;
    let rate_den = u32::try_from(number(settings, "nominal_rate_den")?)?;
    Ok(EncodedStreamInfo {
        capability: EncoderCapability {
            id: settings
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("frame encode probe")
                .to_string(),
            codec: "h264".to_string(),
            // The recorded settings describe a hardware encoder session and its
            // input format; this example does not probe the device itself.
            hardware: settings
                .get("input_format")
                .and_then(Value::as_str)
                .is_some(),
        },
        width: NonZeroU32::new(width).ok_or("the recorded width is zero")?,
        height: NonZeroU32::new(height).ok_or("the recorded height is zero")?,
        nominal_rate_num: NonZeroU32::new(rate_num).ok_or("the recorded rate is zero")?,
        nominal_rate_den: NonZeroU32::new(rate_den)
            .ok_or("the recorded rate denominator is zero")?,
        bitstream_format: BitstreamFormat::H264AnnexB,
        colour: None,
        codec_config,
        allows_reordering: false,
    })
}
