// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! File level behaviour of the single video channel Matroska writer: completed
//! containers, real timestamps, overwrite refusal and the failure policy.

use frame_clock::MediaTimestamp;
use frame_contracts::ErrorCode;
use frame_encode::{BitstreamFormat, EncodedPacket, EncodedStreamInfo, EncoderCapability};
use frame_mux::{ContainerWriter, MkvVideoWriter, interrupted_recordings};
use std::fs;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const SPS: [u8; 8] = [0x67, 0x64, 0x00, 0x3C, 0xAC, 0x2B, 0x40, 0x14];
const PPS: [u8; 4] = [0x68, 0xEE, 0x3C, 0xB0];

/// Element identifiers inspected by the byte level assertions below.
const SEGMENT: [u8; 4] = [0x18, 0x53, 0x80, 0x67];
const CLUSTER: [u8; 4] = [0x1F, 0x43, 0xB6, 0x75];
const CUES: [u8; 4] = [0x1C, 0x53, 0xBB, 0x6B];
const DEFAULT_DURATION: [u8; 3] = [0x23, 0xE3, 0x83];
const CLUSTER_TIMESTAMP: u8 = 0xE7;

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
            "frame-mux-{label}-{}-{nanos}-{}",
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
            id: "integration-test".into(),
            codec: "h264".into(),
            hardware: false,
        },
        width: NonZeroU32::new(640).expect("non-zero"),
        height: NonZeroU32::new(360).expect("non-zero"),
        // A configuration rate that must never appear as a container timestamp.
        nominal_rate_num: NonZeroU32::new(240).expect("non-zero"),
        nominal_rate_den: NonZeroU32::new(1).expect("non-zero"),
        bitstream_format: BitstreamFormat::H264AnnexB,
        colour: None,
        codec_config,
        allows_reordering: false,
    }
}

fn keyframe() -> Vec<u8> {
    vec![0, 0, 0, 1, 0x65, 0x41, 0x42, 0x43]
}

fn delta() -> Vec<u8> {
    vec![0, 0, 0, 1, 0x41, 0x44, 0x45, 0x46]
}

fn packet(track: u32, pts_ns: u64, duration_ns: u64, is_keyframe: bool) -> EncodedPacket {
    EncodedPacket {
        track_id: track,
        pts: MediaTimestamp(pts_ns),
        dts: MediaTimestamp(pts_ns),
        duration_ns,
        keyframe: is_keyframe,
        data: if is_keyframe { keyframe() } else { delta() },
    }
}

/// The irregular intervals of a real capture, in nanoseconds.
const VARIABLE_INTERVALS: [u64; 6] = [
    14_507_800, 16_795_700, 15_500_000, 4_309_000, 66_612_000, 9_995_500,
];

fn decode_vint(data: &[u8], offset: usize) -> Option<(u64, usize)> {
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

/// The Timestamp element of every cluster, found by walking the file's elements with
/// their declared sizes.
fn cluster_timestamps(data: &[u8]) -> Vec<u64> {
    let mut timestamps = Vec::new();
    let mut offset = 0usize;
    while let Some(position) = find(data, &CLUSTER, offset) {
        let Some((size, body)) = decode_vint(data, position + CLUSTER.len()) else {
            break;
        };
        if body < data.len()
            && data[body] == CLUSTER_TIMESTAMP
            && let Some((value_size, value_start)) = decode_vint(data, body + 1)
        {
            let end = value_start + value_size as usize;
            if end <= data.len() {
                let mut value = 0u64;
                for byte in &data[value_start..end] {
                    value = (value << 8) | u64::from(*byte);
                }
                timestamps.push(value);
            }
        }
        offset = body.saturating_add(size as usize).max(position + 1);
    }
    timestamps
}

fn find(data: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= data.len() {
        return None;
    }
    data[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|position| position + from)
}

fn segment_body_length(data: &[u8]) -> (u64, usize) {
    let offset = find(data, &SEGMENT, 0).expect("a segment element");
    let (size, body) = decode_vint(data, offset + SEGMENT.len()).expect("a segment size");
    (size, body)
}

#[test]
fn a_variable_rate_file_is_complete_indexed_and_preserves_source_time() {
    let scratch = Scratch::new("vfr");
    let path = scratch.path("variable.mkv");
    let origin = 160_836_650u64;
    let mut writer =
        MkvVideoWriter::create(&path, &stream_info(), MediaTimestamp(origin)).expect("create");

    // A real capture shape: each frame lasts until the next one arrives, so the
    // duration of frame i is interval i and the last frame keeps the final interval.
    let mut pts = origin;
    for (index, interval) in VARIABLE_INTERVALS.iter().enumerate() {
        writer
            .write(packet(0, pts, *interval, index == 0))
            .expect("packet");
        pts += interval;
    }
    let stats = writer.stats();
    assert_eq!(stats.packets, VARIABLE_INTERVALS.len() as u64);
    assert_eq!(stats.keyframes, 1);
    assert_eq!(stats.first_pts_ns, Some(0));
    let expected_span: u64 = VARIABLE_INTERVALS.iter().sum();
    // Every timestamp and duration carries at most half a container unit of error,
    // and a frame's start and its duration are rounded separately.
    let tolerance = 2 * 500 * VARIABLE_INTERVALS.len() as u64;
    assert!(
        stats.last_end_ns.abs_diff(expected_span) <= tolerance,
        "the container timeline {} must keep the source span {expected_span} within {tolerance} ns",
        stats.last_end_ns
    );

    let artifact = writer.finalize().expect("finalize");
    assert_eq!(artifact.path, path.to_string_lossy());
    assert_eq!(artifact.audio_tracks, 0);
    assert!(
        (artifact.duration_ms - expected_span as f64 / 1_000_000.0).abs() < 0.01,
        "duration {} ms must describe the source span",
        artifact.duration_ms
    );

    let data = fs::read(&path).expect("read back");
    assert_eq!(&data[..4], &[0x1A, 0x45, 0xDF, 0xA3], "EBML magic");
    assert!(
        data.windows(8).any(|window| window == b"matroska"),
        "the file declares the matroska DocType"
    );
    assert!(
        data.windows(15).any(|window| window == b"V_MPEG4/ISO/AVC"),
        "the track declares Matroska AVC"
    );
    let (size, body) = segment_body_length(&data);
    assert_eq!(
        size as usize,
        data.len() - body,
        "the segment size must be patched to the real body, not left unknown"
    );
    assert!(
        find(&data, &DEFAULT_DURATION, 0).is_none(),
        "a variable rate file must not declare one default frame duration"
    );
    assert!(
        find(&data, &CUES, 0).is_some(),
        "keyframes must be indexed for seeking"
    );

    // BlockGroup has no keyframe bit: absence of ReferenceBlock asserts that
    // the frame is independent. Check the container, not ffmpeg's H.264 parser.
    let roots = mkwriter::reader::children(&data);
    let segment = roots
        .iter()
        .find(|element| element.id == 0x18538067)
        .unwrap();
    let dependencies: Vec<bool> = mkwriter::reader::children(segment.body)
        .into_iter()
        .filter(|element| element.id == 0x1F43B675)
        .flat_map(|cluster| mkwriter::reader::children(cluster.body))
        .filter(|element| element.id == 0xA0)
        .map(|group| {
            mkwriter::reader::children(group.body)
                .iter()
                .any(|element| element.id == 0xFB)
        })
        .collect();
    assert_eq!(dependencies, [false, true, true, true, true, true]);

    let clusters = cluster_timestamps(&data);
    assert!(clusters.len() > 1, "the interval pattern spans clusters");
    assert_eq!(clusters[0], 0, "the first cluster starts at the origin");
    assert!(
        clusters.windows(2).all(|pair| pair[0] < pair[1]),
        "cluster timestamps must increase"
    );
    assert!(
        clusters.last().copied().expect("last") <= stats.last_end_ns / 1_000,
        "the last cluster lies inside the container timeline"
    );
}

#[test]
fn aborted_video_is_discovered_but_a_durably_finalized_video_is_not() {
    let scratch = Scratch::new("restart-discovery");
    let abandoned = scratch.path("abandoned.mkv");
    let mut writer = MkvVideoWriter::create(&abandoned, &stream_info(), MediaTimestamp(0)).unwrap();
    writer.write(packet(1, 0, 33_000_000, true)).unwrap();
    assert!(interrupted_recordings(&scratch.dir).unwrap().0.is_empty());
    drop(writer);
    assert_eq!(
        interrupted_recordings(&scratch.dir).unwrap().0,
        vec![abandoned.clone()]
    );
    let original = fs::read(&abandoned).unwrap();
    let completed = scratch.path("completed.mkv");
    let mut writer = MkvVideoWriter::create(&completed, &stream_info(), MediaTimestamp(0)).unwrap();
    writer.write(packet(1, 0, 33_000_000, true)).unwrap();
    writer.finalize().unwrap();
    assert_eq!(
        interrupted_recordings(&scratch.dir).unwrap().0,
        vec![abandoned.clone()]
    );
    assert_eq!(fs::read(abandoned).unwrap(), original);
}

/// Child fixture is run only by the bounded termination test below.
#[test]
#[ignore]
fn journal_process_fixture() {
    let source = PathBuf::from(std::env::var_os("FREYM_JOURNAL_FIXTURE").expect("fixture path"));
    let mut writer = MkvVideoWriter::create(&source, &stream_info(), MediaTimestamp(0)).unwrap();
    // Enough complete clusters to flush the existing buffer without changing
    // production buffering or adding a periodic durability cost.
    for index in 0..1500 {
        writer
            .write(packet(1, index * 33_000_000, 33_000_000, index % 30 == 0))
            .unwrap();
    }
    fs::write(source.with_extension("ready"), b"ready").unwrap();
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

#[test]
fn process_termination_leaves_discoverable_media_and_recovery_preserves_the_source() {
    let scratch = Scratch::new("process-termination");
    let source = scratch.path("interrupted.mkv");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "journal_process_fixture", "--ignored"])
        .env("FREYM_JOURNAL_FIXTURE", &source)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !source.with_extension("ready").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let ready = source.with_extension("ready").exists();
    let active_scan = interrupted_recordings(&scratch.dir);
    let kill_result = child.kill();
    let status = child.wait().unwrap();
    assert!(
        ready,
        "fixture must reach its owned recording before termination"
    );
    kill_result.unwrap();
    assert!(!status.success());
    assert!(active_scan.unwrap().0.is_empty());
    assert_eq!(
        interrupted_recordings(&scratch.dir).unwrap().0,
        vec![source.clone()]
    );
    let original = fs::read(&source).unwrap();
    let recovered = scratch.path("recovered.mkv");
    let result = frame_mux::recover(&source, &recovered).unwrap();
    assert!(result.recovered_packets > 0);
    assert_eq!(fs::read(&source).unwrap(), original);
    assert!(recovered.exists());
}

#[test]
fn an_existing_recording_is_never_overwritten() {
    let scratch = Scratch::new("overwrite");
    let path = scratch.path("keep.mkv");
    let original = b"an existing recording";
    fs::write(&path, original).expect("seed file");

    let error = MkvVideoWriter::create(&path, &stream_info(), MediaTimestamp(0))
        .expect_err("an existing path is refused");
    assert_eq!(error.code, ErrorCode::InvalidOutputPath);
    assert!(error.message.contains("keep.mkv"));
    assert_eq!(
        fs::read(&path).expect("read back"),
        original,
        "the refused create must leave the existing bytes untouched"
    );
}

#[test]
fn a_missing_directory_is_reported_as_an_output_path_error() {
    let scratch = Scratch::new("missing-directory");
    let path = scratch.path("absent").join("out.mkv");
    let error = MkvVideoWriter::create(&path, &stream_info(), MediaTimestamp(0))
        .expect_err("the parent directory does not exist");
    assert_eq!(error.code, ErrorCode::InvalidOutputPath);
}

#[test]
fn an_invalid_timeline_is_refused_and_prevents_finalization() {
    let scratch = Scratch::new("timeline");
    let path = scratch.path("invalid.mkv");
    let mut writer =
        MkvVideoWriter::create(&path, &stream_info(), MediaTimestamp(0)).expect("create");
    writer
        .write(packet(0, 16_683_334, 16_683_334, true))
        .expect("a valid first packet");

    let error = writer
        .write(packet(0, 16_683_334, 16_683_334, false))
        .expect_err("a repeated timestamp is refused");
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(error.message.contains("cannot hold two frames"));
    let stored = writer
        .finalize()
        .expect_err("a failed writer never produces an artifact");
    assert_eq!(
        stored.message, error.message,
        "the first failure is preserved"
    );
}

#[test]
fn a_second_track_and_a_zero_duration_are_refused() {
    let scratch = Scratch::new("requests");
    let mut writer = MkvVideoWriter::create(
        &scratch.path("track.mkv"),
        &stream_info(),
        MediaTimestamp(0),
    )
    .expect("create");
    writer
        .write(packet(0, 16_683_334, 16_683_334, true))
        .expect("a valid first packet");
    let error = writer
        .write(packet(1, 33_366_668, 16_683_334, false))
        .expect_err("a second track is refused");
    assert!(error.message.contains("track 1"));
    assert!(writer.finalize().is_err());

    let mut writer = MkvVideoWriter::create(
        &scratch.path("duration.mkv"),
        &stream_info(),
        MediaTimestamp(0),
    )
    .expect("create");
    let error = writer
        .write(packet(0, 0, 0, true))
        .expect_err("a zero duration is refused");
    assert!(error.message.contains("zero duration"));
    assert!(writer.finalize().is_err());
}

#[test]
fn two_runs_of_the_same_input_write_the_same_bytes() {
    let scratch = Scratch::new("deterministic");
    let first = scratch.path("first.mkv");
    let second = scratch.path("second.mkv");
    for path in [&first, &second] {
        let mut writer =
            MkvVideoWriter::create(path, &stream_info(), MediaTimestamp(1_000)).expect("create");
        writer
            .write(packet(0, 2_000, 33_366_667, true))
            .expect("keyframe");
        writer
            .write(packet(0, 35_366_667, 33_366_667, false))
            .expect("delta");
        writer.finalize().expect("finalize");
    }
    assert_eq!(
        fs::read(&first).expect("first"),
        fs::read(&second).expect("second"),
        "no random segment or track identifier may reach the file"
    );
}

#[test]
fn a_packet_larger_than_the_limit_is_refused_before_it_is_written() {
    let scratch = Scratch::new("oversized");
    let path = scratch.path("oversized.mkv");
    let mut writer =
        MkvVideoWriter::create(&path, &stream_info(), MediaTimestamp(0)).expect("create");
    let mut oversized = packet(0, 0, 1_000, true);
    oversized.data = vec![0x41u8; frame_mux::MAX_PACKET_BYTES + 1];
    let error = writer.write(oversized).expect_err("the limit is enforced");
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(writer.failed());
    assert!(writer.finalize().is_err());
    assert!(
        fs::metadata(&path).is_ok_and(|metadata| metadata.len() > 0),
        "the partial file is retained for diagnosis and never reported as an artifact"
    );
}
