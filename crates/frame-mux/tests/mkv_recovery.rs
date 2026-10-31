// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! File level behaviour of Matroska recovery.
//!
//! Three scenarios, each on a container the real writer produced:
//!
//! 1. a copy cut inside its last cluster recovers exactly the complete prefix, and
//!    reports the bytes it refused to interpret;
//! 2. the complete file is remuxed with byte-identical block payloads, unchanged
//!    container timestamps, unchanged padding and the writer's own duration;
//! 3. corrupt codec metadata is refused without leaving an output file.
//!
//! Recovered streams can be decoded by external tools for separate media acceptance;
//! these tests assert the container facts those decodes rest on.

use frame_audio::{TrackKind, TrackPlan};
use frame_clock::MediaTimestamp;
use frame_contracts::{AudioSourceId, ErrorCode};
use frame_encode::opus::{OpusTrackEncoder, SAMPLE_RATE};
use frame_encode::{BitstreamFormat, EncodedPacket, EncodedStreamInfo, EncoderCapability};
use frame_mux::{AvAudioTrack, MkvAvWriter, TailKind, recover};
use std::fs;
use std::num::{NonZeroU16, NonZeroU32};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const SPS: [u8; 8] = [0x67, 0x64, 0x00, 0x3C, 0xAC, 0x2B, 0x40, 0x14];
const PPS: [u8; 4] = [0x68, 0xEE, 0x3C, 0xB0];

const SEGMENT_ID: u32 = 0x1853_8067;
const INFO_ID: u32 = 0x1549_A966;
const CLUSTER_ID: u32 = 0x1F43_B675;
const TIMESTAMP_ID: u32 = 0xE7;
const SIMPLE_BLOCK_ID: u32 = 0xA3;
const BLOCK_GROUP_ID: u32 = 0xA0;
const BLOCK_ID: u32 = 0xA1;
const BLOCK_DURATION_ID: u32 = 0x9B;
const REFERENCE_BLOCK_ID: u32 = 0xFB;
const DISCARD_PADDING_ID: u32 = 0x75A2;
const TIMESTAMP_SCALE_ID: u32 = 0x002A_D7B1;
const DURATION_ID: u32 = 0x4489;
const CUES_ID: u32 = 0x1C53_BB6B;
const SEEK_HEAD_ID: u32 = 0x114D_9B74;
const VOID_ID: u32 = 0xEC;

/// 48 kHz stereo, the only layout the Opus writer declares.
const RATE: u32 = SAMPLE_RATE;
const CHANNELS: u16 = 2;
/// The mixer's ten-millisecond quantum.
const QUANTUM_FRAMES: usize = 480;
/// 20 quanta of synthetic audio: 200 ms of input.
const QUANTA: u64 = 20;
/// Video access units at 33 ms, two of them keyframes.
const VIDEO_FRAMES: u64 = 12;
const VIDEO_FRAME_NS: u64 = 33_000_000;

struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "frame-mux-recovery-{label}-{}-{nanos}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("scratch directory");
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn stream_info() -> EncodedStreamInfo {
    let mut codec_config = Vec::new();
    codec_config.extend_from_slice(&[0, 0, 0, 1]);
    codec_config.extend_from_slice(&SPS);
    codec_config.extend_from_slice(&[0, 0, 0, 1]);
    codec_config.extend_from_slice(&PPS);
    EncodedStreamInfo {
        capability: EncoderCapability {
            id: "recovery-test".into(),
            codec: "h264".into(),
            hardware: false,
        },
        width: NonZeroU32::new(64).expect("non-zero"),
        height: NonZeroU32::new(48).expect("non-zero"),
        nominal_rate_num: NonZeroU32::new(30).expect("non-zero"),
        nominal_rate_den: NonZeroU32::new(1).expect("non-zero"),
        bitstream_format: BitstreamFormat::H264AnnexB,
        colour: None,
        codec_config,
        allows_reordering: false,
    }
}

fn video_packet(pts_ns: u64, index: u64) -> EncodedPacket {
    let keyframe = index.is_multiple_of(6);
    let mut unit = Vec::new();
    unit.extend_from_slice(&[0, 0, 0, 1]);
    unit.extend_from_slice(&SPS);
    unit.extend_from_slice(&[0, 0, 0, 1]);
    unit.extend_from_slice(&PPS);
    unit.extend_from_slice(&[0, 0, 0, 1, if keyframe { 0x65 } else { 0x41 }]);
    unit.extend_from_slice(&[0xAAu8.wrapping_add(index as u8); 64]);
    EncodedPacket {
        track_id: 0,
        pts: MediaTimestamp(pts_ns),
        dts: MediaTimestamp(pts_ns),
        duration_ns: VIDEO_FRAME_NS,
        keyframe,
        data: unit,
    }
}

/// One real Opus track: 440 Hz stereo through the crate's own encoder.
fn encode_track(origin_ns: u64) -> (Vec<EncodedPacket>, u16, u16) {
    let mut encoder = OpusTrackEncoder::new(0, MediaTimestamp(origin_ns)).expect("an Opus encoder");
    let pre_skip = encoder.pre_skip();
    let mut packets = Vec::new();
    for quantum in 0..QUANTA {
        let start = quantum * QUANTUM_FRAMES as u64;
        let mut samples = Vec::with_capacity(QUANTUM_FRAMES * CHANNELS as usize);
        for offset in 0..QUANTUM_FRAMES as u64 {
            let frame = start + offset;
            let phase = 2.0 * std::f64::consts::PI * 440.0 * frame as f64 / f64::from(SAMPLE_RATE);
            let value = (phase.sin() * 0.5) as f32;
            samples.push(value);
            samples.push(value);
        }
        let block = frame_audio::TrackPcm {
            track_index: 0,
            timestamp: MediaTimestamp(origin_ns + start * 1_000_000_000 / u64::from(SAMPLE_RATE)),
            sample_rate: NonZeroU32::new(RATE).expect("non-zero"),
            channels: NonZeroU16::new(CHANNELS).expect("non-zero"),
            samples,
        };
        if let Some(packet) = encoder.submit(&block).expect("aligned PCM") {
            packets.push(packet);
        }
    }
    let finish = encoder.finish().expect("the encoder flushes");
    packets.extend(finish.packets);
    (packets, pre_skip, finish.discard_padding)
}

/// Writes one real H.264 + Opus container and returns its path.
fn write_scenario(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    let origin = 0u64;
    let (audio, pre_skip, discard_padding) = encode_track(origin);
    let plan = TrackPlan {
        kind: TrackKind::MainMix,
        sources: vec![AudioSourceId("mix".into())],
    };
    let track = AvAudioTrack::from_plan(&plan, pre_skip).expect("a named track");
    let mut writer =
        MkvAvWriter::create(&path, &stream_info(), &[track], MediaTimestamp(origin)).expect("new");
    let mut audio = audio.into_iter().peekable();
    let mut frame = 0u64;
    loop {
        let video_pts = (frame < VIDEO_FRAMES).then(|| frame * VIDEO_FRAME_NS);
        let audio_pts = audio.peek().map(|packet| packet.pts.0);
        let next = match (video_pts, audio_pts) {
            (Some(video), Some(sample)) => video.min(sample),
            (Some(video), None) => video,
            (None, Some(sample)) => sample,
            (None, None) => break,
        };
        if video_pts == Some(next) {
            writer
                .write_video(video_packet(frame * VIDEO_FRAME_NS, frame))
                .expect("a video packet");
            frame += 1;
        } else {
            let packet = audio.next().expect("an audio packet");
            writer.write_audio(packet).expect("an audio packet");
        }
    }
    assert_eq!(frame, VIDEO_FRAMES, "every video frame is delivered");
    assert!(audio.next().is_none(), "every audio packet is delivered");
    writer
        .finish_audio(0, discard_padding)
        .expect("the track ends with its own padding");
    writer.finalize().expect("finalize");
    path
}

fn read_vint(data: &[u8], offset: usize) -> Option<(u64, usize)> {
    let first = *data.get(offset)?;
    let mut length = 1;
    let mut mask = 0x80;
    while length <= 8 && first & mask == 0 {
        mask >>= 1;
        length += 1;
    }
    if length > 8 {
        return None;
    }
    let mut value = u64::from(first & (mask - 1));
    for index in 1..length {
        value = (value << 8) | u64::from(*data.get(offset + index)?);
    }
    Some((value, offset + length))
}

/// One element identifier: stored raw, its width given by its first byte.
fn read_id(data: &[u8], offset: usize) -> (u32, usize) {
    let first = data[offset];
    let length = first.leading_zeros() as usize + 1;
    let mut value = 0u32;
    for index in 0..length {
        value = (value << 8) | u32::from(data[offset + index]);
    }
    (value, offset + length)
}

/// One element header: identifier, body start and declared body length.
fn next_element(data: &[u8], offset: usize) -> (u32, usize, u64) {
    let (id, after_id) = read_id(data, offset);
    let (size, after_size) = read_vint(data, after_id).expect("an element size");
    let unknown = size == (1u64 << (7 * (after_size - after_id) as u32)) - 1;
    let length = if unknown {
        (data.len() - after_size) as u64
    } else {
        size
    };
    (id, after_size, length)
}

/// One recovered block, with everything the container said about it.
#[derive(Debug, PartialEq, Eq)]
struct Block {
    track: u64,
    pts_units: u64,
    flags: u8,
    payload: Vec<u8>,
    duration: Option<u64>,
    reference_block: Option<Vec<u8>>,
    discard_padding: Option<Vec<u8>>,
}

fn segments(data: &[u8]) -> Vec<mkwriter::reader::Element<'_>> {
    mkwriter::reader::children(data)
        .into_iter()
        .filter(|element| element.id == SEGMENT_ID)
        .collect()
}

/// Every block of the file, in on-disk delivery order.
fn blocks(data: &[u8]) -> Vec<Block> {
    let mut out = Vec::new();
    for segment in segments(data) {
        for cluster in mkwriter::reader::children(segment.body)
            .into_iter()
            .filter(|element| element.id == CLUSTER_ID)
        {
            let children = mkwriter::reader::children(cluster.body);
            let base = mkwriter::reader::as_uint(
                children
                    .iter()
                    .find(|element| element.id == TIMESTAMP_ID)
                    .expect("a cluster timestamp")
                    .body,
            );
            for element in children
                .iter()
                .filter(|element| element.id == BLOCK_GROUP_ID || element.id == SIMPLE_BLOCK_ID)
            {
                let (block, duration, reference, padding) = if element.id == BLOCK_GROUP_ID {
                    let fields = mkwriter::reader::children(element.body);
                    let block = fields
                        .iter()
                        .find(|field| field.id == BLOCK_ID)
                        .expect("a block")
                        .body;
                    let duration = fields
                        .iter()
                        .find(|field| field.id == BLOCK_DURATION_ID)
                        .map(|field| mkwriter::reader::as_uint(field.body));
                    let reference = fields
                        .iter()
                        .find(|field| field.id == REFERENCE_BLOCK_ID)
                        .map(|field| field.body.to_vec());
                    let padding = fields
                        .iter()
                        .find(|field| field.id == DISCARD_PADDING_ID)
                        .map(|field| field.body.to_vec());
                    (block, duration, reference, padding)
                } else {
                    (element.body, None, None, None)
                };
                let (track, after_track) = read_vint(block, 0).expect("a track number");
                let relative = i16::from_be_bytes([block[after_track], block[after_track + 1]]);
                out.push(Block {
                    track,
                    pts_units: (base as i64 + i64::from(relative)) as u64,
                    flags: block[after_track + 2],
                    payload: block[after_track + 3..].to_vec(),
                    duration,
                    reference_block: reference,
                    discard_padding: padding,
                });
            }
        }
    }
    out
}

/// The `Duration` the file declares, in nanoseconds.
fn declared_duration_ns(data: &[u8]) -> u64 {
    let segment = segments(data).into_iter().next().expect("a segment");
    let info = mkwriter::reader::children(segment.body)
        .into_iter()
        .find(|element| element.id == INFO_ID)
        .expect("an info element");
    let fields = mkwriter::reader::children(info.body);
    let scale = mkwriter::reader::as_uint(
        fields
            .iter()
            .find(|field| field.id == TIMESTAMP_SCALE_ID)
            .expect("a timestamp scale")
            .body,
    );
    let duration = fields
        .iter()
        .find(|field| field.id == DURATION_ID)
        .expect("a duration");
    (f64::from_be_bytes(duration.body.try_into().expect("eight bytes")) * scale as f64) as u64
}

/// True when the segment declares exactly the bytes the file holds after it.
fn segment_declares_its_length(data: &[u8]) -> bool {
    let (first, first_body, first_size) = next_element(data, 0);
    assert_eq!(first, 0x1A45_DFA3, "the EBML header comes first");
    let (second, second_body, second_size) = next_element(data, first_body + first_size as usize);
    assert_eq!(second, SEGMENT_ID, "the segment follows the EBML header");
    second_size as usize == data.len() - second_body
}

fn element_count(data: &[u8], wanted: u32) -> usize {
    let mut total = 0;
    let mut offset = 0;
    while offset < data.len() {
        let (id, body, size) = next_element(data, offset);
        if id == wanted {
            total += 1;
        }
        if id == 0x1A45_DFA3 || id == SEGMENT_ID || id == INFO_ID {
            total += element_count(&data[body..body + size as usize], wanted);
        }
        offset = body + size as usize;
        if size == 0 && id != VOID_ID {
            break;
        }
    }
    total
}

fn assert_unchanged(path: &Path, expected: &[u8]) {
    assert_eq!(
        fs::read(path).expect("read back"),
        expected,
        "{} must never be modified by recovery",
        path.display()
    );
}

#[test]
fn a_copy_cut_inside_its_last_cluster_recovers_the_complete_prefix() {
    let scratch = Scratch::new("cut");
    let complete = write_scenario(&scratch.dir, "complete.mkv");
    let full = fs::read(&complete).expect("read back");

    // Cut a copy inside the last cluster, which is what an interrupted write leaves.
    let segment = segments(&full).into_iter().next().expect("a segment");
    let clusters: Vec<mkwriter::reader::Element> = mkwriter::reader::children(segment.body)
        .into_iter()
        .filter(|element| element.id == CLUSTER_ID)
        .collect();
    assert!(
        clusters.len() > 1,
        "the scenario spans more than one cluster"
    );
    let last = clusters.last().expect("a last cluster");
    let body_start = last.body.as_ptr() as usize - full.as_ptr() as usize;
    let cut = body_start + last.body.len() / 2;
    assert!(cut < full.len(), "the cluster body is not the whole file");
    let partial = scratch.path("cut.mkv");
    fs::write(&partial, &full[..cut]).expect("write the copy");

    let recovered = scratch.path("recovered.mkv");
    let summary = recover(&partial, &recovered).expect("the prefix recovers");

    assert_eq!(summary.source_bytes, cut as u64);
    assert_eq!(summary.dropped_incomplete_packets, None);
    assert_eq!(summary.unknown_tail.kind, TailKind::Truncated);
    assert!(
        summary.unknown_tail.bytes > 0,
        "the cut leaves an unparsed tail"
    );
    assert_eq!(
        summary.recovered_bytes + summary.dropped_incomplete_bytes,
        summary.source_bytes
    );
    assert_eq!(
        summary.unknown_tail.start_offset, summary.recovered_bytes,
        "the tail starts where the covered prefix ends"
    );
    assert!(
        summary
            .unknown_tail
            .reason
            .as_deref()
            .is_some_and(|reason| !reason.is_empty())
    );
    assert!(!summary.source_finalized, "a cut copy was never finalized");
    assert!(
        !summary.final_trim_known,
        "the encoder's final trim is unknown"
    );
    assert!(summary.is_playable_prefix());
    assert_eq!(summary.cues, summary.recovered_keyframes);
    assert!(summary.recovered_video_packets > 0);
    assert!(summary.recovered_audio_packets > 0);

    // The recovered blocks are exactly the complete prefix of the original, in the
    // same order, with the same container timestamps and the same payloads.
    let expected = blocks(&full);
    let got = blocks(&fs::read(&recovered).expect("read the recovered file"));
    let count = usize::try_from(summary.recovered_packets).expect("a packet count");
    assert!(count > 0 && count < expected.len(), "the cut drops packets");
    assert_eq!(got, expected[..count], "the complete prefix is preserved");
    for (index, block) in got.iter().enumerate() {
        assert_eq!(block.pts_units, expected[index].pts_units, "block {index}");
    }
    assert!(summary.recovered_keyframes >= 1);
    assert!(summary.recovered_keyframes < VIDEO_FRAMES);
    assert!(
        summary
            .skipped_elements
            .iter()
            .all(|element| matches!(element.id, SEEK_HEAD_ID | CUES_ID | VOID_ID | 0xBF)),
        "only elements without media are skipped: {:?}",
        summary.skipped_elements
    );
    assert!(
        summary
            .skipped_elements
            .iter()
            .any(|element| element.id == SEEK_HEAD_ID),
        "the source's own index is never trusted and is reported"
    );
    assert!(
        summary
            .notes
            .iter()
            .any(|note| note.contains("complete prefix only")),
        "the summary states that recovery is not completion"
    );

    // The new container is complete: closed cluster stream, index, seek head and a
    // segment that declares exactly its own length.
    let data = fs::read(&recovered).expect("read back");
    assert_eq!(data.len() as u64, summary.output_bytes);
    assert!(segment_declares_its_length(&data));
    assert!(
        element_count(&data, CUES_ID) > 0,
        "the index was regenerated"
    );
    assert!(
        element_count(&data, SEEK_HEAD_ID) > 0,
        "the seek head was regenerated"
    );
    assert_eq!(blocks(&data).len(), count);

    // Neither file changed the other.
    assert_unchanged(&partial, &full[..cut]);
    assert_unchanged(&complete, &full);
}

#[test]
fn a_complete_recording_is_remuxed_with_its_times_payloads_and_duration() {
    let scratch = Scratch::new("complete");
    let source = write_scenario(&scratch.dir, "complete.mkv");
    let full = fs::read(&source).expect("read back");
    let recovered = scratch.path("recovered.mkv");

    let summary = recover(&source, &recovered).expect("a complete file recovers");
    assert_eq!(summary.unknown_tail.kind, TailKind::None);
    assert_eq!(summary.unknown_tail.bytes, 0);
    assert_eq!(summary.unknown_tail.reason, None);
    assert_eq!(summary.recovered_bytes, summary.source_bytes);
    assert_eq!(summary.dropped_incomplete_bytes, 0);
    assert!(summary.source_finalized);
    assert!(summary.final_trim_known);
    assert_eq!(
        summary.recovered_packets as usize,
        blocks(&full).len(),
        "every block is recovered"
    );
    assert_eq!(
        summary.recovered_video_packets, VIDEO_FRAMES,
        "every video packet is recovered"
    );
    assert_eq!(summary.recovered_keyframes, 2, "two keyframes, two cues");
    assert_eq!(summary.cues, 2);
    assert!(summary.is_playable_prefix());

    // Every block, including its padding and dependency mark, is byte-identical and
    // keeps its container timestamp.
    let expected = blocks(&full);
    let data = fs::read(&recovered).expect("read back");
    let got = blocks(&data);
    assert_eq!(got, expected, "the remux preserves packets and timestamps");
    assert!(got.iter().any(|block| block.discard_padding.is_some()));
    assert!(got.iter().any(|block| block.reference_block.is_some()));

    // The duration the writer's own finalization stored is reproduced exactly.
    assert_eq!(summary.duration_ns, declared_duration_ns(&full));
    assert_eq!(
        summary.source_declared_duration_ns,
        Some(summary.duration_ns)
    );
    assert!(summary.container_end_ns >= summary.duration_ns);

    // The remuxed container is complete and the source is untouched.
    assert_eq!(data.len() as u64, summary.output_bytes);
    assert!(segment_declares_its_length(&data));
    assert_eq!(element_count(&data, CUES_ID), element_count(&full, CUES_ID));
    assert_unchanged(&source, &full);

    // A complete recording is reproduced byte for byte: the same cluster framing, the
    // same block groups, the same index and the same header fields. Those bytes cover
    // timestamps, payloads, padding and track metadata at once.
    assert_eq!(data, full, "a complete recording remuxes byte for byte");

    // The recovered track keeps its identity, configuration and delay chain.
    let track = &summary.tracks[0];
    assert_eq!(track.number, 1);
    assert_eq!(track.kind.as_str(), "video");
    assert_eq!(track.codec_id, "V_MPEG4/ISO/AVC");
    let audio = &summary.tracks[1];
    assert_eq!(audio.number, 2);
    assert_eq!(audio.kind.as_str(), "audio");
    assert_eq!(audio.codec_id, "A_OPUS");
    assert_eq!(audio.name.as_deref(), Some("main"));
    assert_eq!(audio.seek_pre_roll_ns, Some(80_000_000));
    assert!(audio.codec_delay_ns.is_some());
    assert_eq!(audio.packets, summary.recovered_audio_packets);
}

#[test]
fn corrupt_codec_metadata_is_refused_without_an_artifact() {
    let scratch = Scratch::new("corrupt");
    let source = write_scenario(&scratch.dir, "complete.mkv");
    let mut data = fs::read(&source).expect("read back");

    // Damage the video track's AVC configuration record: a reader can still find the
    // element, but the metadata it holds is no longer usable.
    let position = data
        .windows(2)
        .position(|window| window == [0x63, 0xA2])
        .expect("the video codec private element");
    assert_eq!(
        data[position + 2],
        0x80 | 23,
        "the video configuration's size"
    );
    assert_eq!(data[position + 3], 1, "configuration version");
    data[position + 3] = 2;

    let corrupt = scratch.path("corrupt.mkv");
    fs::write(&corrupt, &data).expect("write the damaged copy");
    let output = scratch.path("recovered.mkv");
    let error = recover(&corrupt, &output).expect_err("unusable metadata is refused");
    assert_eq!(error.code, ErrorCode::UnsupportedCodec);
    assert!(
        error.message.contains("configuration version 2"),
        "{}",
        error.message
    );
    assert!(
        !output.exists(),
        "a refused recovery leaves no output file behind"
    );

    // The same holds for a video track that declares no configuration at all.
    let mut without_private = fs::read(&source).expect("read back");
    let position = without_private
        .windows(2)
        .position(|window| window == [0x63, 0xA2])
        .expect("the video codec private element");
    without_private[position] = 0x63;
    without_private[position + 1] = 0x00;
    let absent = scratch.path("absent.mkv");
    fs::write(&absent, &without_private).expect("write the damaged copy");
    let output = scratch.path("recovered-2.mkv");
    let error = recover(&absent, &output).expect_err("a track without configuration is refused");
    assert_eq!(error.code, ErrorCode::UnsupportedCodec);
    assert!(!output.exists());

    // Neither damaged copy was touched, and neither damaged copy was written to.
    assert_unchanged(&corrupt, &data);
}

#[test]
fn an_existing_output_is_never_overwritten() {
    let scratch = Scratch::new("overwrite");
    let source = write_scenario(&scratch.dir, "complete.mkv");
    let output = scratch.path("keep.mkv");
    let existing = b"an existing recovered file".to_vec();
    fs::write(&output, &existing).expect("seed the output");

    let error = recover(&source, &output).expect_err("an existing output is refused");
    assert_eq!(error.code, ErrorCode::InvalidOutputPath);
    assert!(error.message.contains("keep.mkv"), "{}", error.message);
    assert_unchanged(&output, &existing);
}
