// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Writes one real H.264 proof stream and two Opus tracks into one Matroska file.
//!
//! The `.h264` file and its `.jsonl` sidecar are opened read-only and consumed packet
//! by packet: the sidecar supplies each packet's byte range, PTS, DTS, duration and
//! keyframe flag exactly as the hardware encoder reported them, and the elementary
//! stream supplies the bytes. No video frame is decoded, re-encoded or retimed, and no
//! nominal frame rate is applied to the timeline.
//!
//! The audio is synthesized here: a 440 Hz main mix and a 1 kHz clean stem at 48 kHz
//! stereo, encoded by the real `OpusTrackEncoder`. Nothing external is executed and no
//! container is muxed by another tool.
//!
//! ```text
//! cargo run --locked -p frame-mux --example write_av_mkv -- \
//!     --input <stream.h264> --output <new.mkv> [--sidecar <stream.h264.jsonl>] \
//!     [--origin-ns <ns>]
//! ```
//!
//! The output path is created exclusively, so an existing recording is never
//! overwritten. `--origin-ns` defaults to the first video packet's PTS.

use frame_audio::{TrackKind, TrackPlan};
use frame_clock::MediaTimestamp;
use frame_contracts::AudioSourceId;
use frame_encode::opus::{FRAME_SAMPLES, OpusTrackEncoder, SAMPLE_RATE};
use frame_encode::{BitstreamFormat, EncodedPacket, EncodedStreamInfo, EncoderCapability};
use frame_mux::{AvAudioTrack, MkvAvWriter};
use serde_json::{Value, json};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::num::{NonZeroU16, NonZeroU32};
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const USAGE: &str = "write_av_mkv --input <stream.h264> --output <new.mkv> [--sidecar <stream.h264.jsonl>] [--origin-ns <ns>]";
/// The mixer's ten-millisecond quantum, the largest block the Opus encoder accepts.
const QUANTUM_FRAMES: usize = 480;
/// How many quanta of synthetic audio to encode per track.
const QUANTA: u64 = 300;
const CHANNELS: u16 = 2;

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

    let mut file = BufReader::new(File::open(&input)?);
    let first = sidecar
        .packets
        .first()
        .ok_or("the sidecar lists no packets")?;
    let origin_ns = origin_ns.unwrap_or(first.pts_ns);

    // The sidecar records the codec configuration's size, not its bytes, so the
    // parameter sets are recovered from the access unit the encoder emitted first.
    let first_data = read_packet(&mut file, first)?;
    let codec_config = leading_parameter_sets(&first_data)?;
    let stream = stream_info(&sidecar.settings, codec_config)?;

    // Two really encoded Opus tracks: a 440 Hz main mix and a 1 kHz clean stem.
    let plans = [
        TrackPlan {
            kind: TrackKind::MainMix,
            sources: vec![AudioSourceId("main".into())],
        },
        TrackPlan {
            kind: TrackKind::Clean(AudioSourceId("stem".into())),
            sources: vec![AudioSourceId("stem".into())],
        },
    ];
    let encoded = [
        encode_track(0, 440.0, origin_ns)?,
        encode_track(1, 1_000.0, origin_ns)?,
    ];
    let tracks: Vec<AvAudioTrack> = plans
        .iter()
        .zip(&encoded)
        .map(|(plan, track)| AvAudioTrack::from_plan(plan, track.pre_skip))
        .collect::<std::result::Result<_, _>>()?;

    let mut writer = MkvAvWriter::create(&output, &stream, &tracks, MediaTimestamp(origin_ns))?;

    // One global non-decreasing pass over the video timeline and the audio queues.
    let pre_skip: Vec<u16> = encoded.iter().map(|track| track.pre_skip).collect();
    let discard_padding: Vec<u16> = encoded.iter().map(|track| track.discard_padding).collect();
    let audio_input_frames: Vec<u64> = encoded.iter().map(|track| track.input_frames).collect();
    let mut queues: Vec<std::vec::IntoIter<EncodedPacket>> = encoded
        .into_iter()
        .map(|track| track.packets.into_iter())
        .collect();
    let mut position = 0usize;
    loop {
        let video_pts = sidecar.packets.get(position).map(|packet| packet.pts_ns);
        let audio_pts = queues
            .iter_mut()
            .filter_map(|queue| queue.as_slice().first().map(|packet| packet.pts.0))
            .min();
        let next = match (video_pts, audio_pts) {
            (Some(video), Some(sample)) => video.min(sample),
            (Some(video), None) => video,
            (None, Some(sample)) => sample,
            (None, None) => break,
        };
        if video_pts == Some(next) {
            let packet = &sidecar.packets[position];
            let data = if position == 0 {
                first_data.clone()
            } else {
                read_packet(&mut file, packet)?
            };
            writer.write_video(EncodedPacket {
                // The single video track the hardware encoder emits.
                track_id: 0,
                pts: MediaTimestamp(packet.pts_ns),
                dts: MediaTimestamp(packet.dts_ns),
                duration_ns: packet.duration_ns,
                keyframe: packet.keyframe,
                data,
            })?;
            position += 1;
            continue;
        }
        let track = queues
            .iter_mut()
            .position(|queue| {
                queue
                    .as_slice()
                    .first()
                    .is_some_and(|packet| packet.pts.0 == next)
            })
            .expect("a queue holds the earliest audio timestamp");
        let packet = queues[track].next().expect("front exists");
        writer.write_audio(packet)?;
    }
    if position != sidecar.packets.len() {
        return Err("not every sidecar packet was written".into());
    }

    for (index, padding) in discard_padding.iter().enumerate() {
        writer.finish_audio(index, *padding)?;
    }
    let stats = writer.stats();
    let artifact = writer.finalize()?;
    let report = json!({
        "artifact": artifact,
        "origin_ns": origin_ns,
        "input": input.to_string_lossy(),
        "sidecar": sidecar_path.to_string_lossy(),
        "video_packets": stats.video_packets,
        "video_keyframes": stats.video_keyframes,
        "audio_tracks": stats.audio_tracks,
        "audio_packets": stats.audio_packets,
        "audio_finished_tracks": stats.audio_finished_tracks,
        "container_end_ns": stats.last_end_ns,
        "trimmed_end_ns": stats.trimmed_end_ns,
        "first_video_pts_ns": stats.first_video_pts_ns,
        "retained_bytes_estimate": stats.retained_bytes_estimate,
        "pre_skip": pre_skip,
        "discard_padding": discard_padding,
        "audio_input_frames": audio_input_frames,
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

struct EncodedTrack {
    packets: Vec<EncodedPacket>,
    pre_skip: u16,
    discard_padding: u16,
    input_frames: u64,
}

/// Encodes one synthetic stereo tone through the real Opus encoder.
fn encode_track(track: usize, frequency: f64, origin_ns: u64) -> Result<EncodedTrack> {
    let mut encoder = OpusTrackEncoder::new(track, MediaTimestamp(origin_ns))?;
    let pre_skip = encoder.pre_skip();
    let mut packets = Vec::new();
    for quantum in 0..QUANTA {
        let start = quantum * QUANTUM_FRAMES as u64;
        let mut samples = Vec::with_capacity(QUANTUM_FRAMES * CHANNELS as usize);
        for offset in 0..QUANTUM_FRAMES as u64 {
            let frame = start + offset;
            let phase =
                2.0 * std::f64::consts::PI * frequency * frame as f64 / f64::from(SAMPLE_RATE);
            let value = (phase.sin() * 0.5) as f32;
            samples.push(value);
            samples.push(value);
        }
        let block = frame_audio::TrackPcm {
            track_index: track,
            timestamp: MediaTimestamp(origin_ns + start * 1_000_000_000 / u64::from(SAMPLE_RATE)),
            sample_rate: NonZeroU32::new(SAMPLE_RATE).ok_or("48 kHz is nonzero")?,
            channels: NonZeroU16::new(CHANNELS).ok_or("stereo is nonzero")?,
            samples,
        };
        if let Some(packet) = encoder.submit(&block)? {
            packets.push(packet);
        }
    }
    let finish = encoder.finish()?;
    packets.extend(finish.packets);
    if packets.len() as u64
        != (finish.input_frames + u64::from(finish.pre_skip)).div_ceil(FRAME_SAMPLES as u64)
    {
        return Err("the Opus packet count does not cover the input".into());
    }
    Ok(EncodedTrack {
        packets,
        pre_skip,
        discard_padding: finish.discard_padding,
        input_frames: finish.input_frames,
    })
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
            _ => {}
        }
    }
    Ok(Sidecar {
        settings: settings.ok_or("the sidecar has no settings record")?,
        packets,
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
