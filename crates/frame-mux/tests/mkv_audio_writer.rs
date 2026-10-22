// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! File level behaviour of the audio/video Matroska writer.
//!
//! One scenario covers what the container must hold: real H.264 access units, two
//! genuinely Opus-encoded synthetic PCM tracks with different content, the full
//! `OpusHead`/`CodecDelay`/`SeekPreRoll`/`DiscardPadding` chain, the overwrite
//! refusal and the refusal to produce an artifact while a declared audio track is
//! unfinished or empty.
//!
//! The two PCM tracks are encoded by the real `OpusTrackEncoder`, so the packets,
//! timestamps, pre-skip and discard padding are the codec's own, not fixtures.

use frame_audio::{TrackKind, TrackPlan};
use frame_clock::MediaTimestamp;
use frame_contracts::{AudioSourceId, ErrorCode};
use frame_encode::opus::{FRAME_SAMPLES, OpusTrackEncoder, SAMPLE_RATE};
use frame_encode::{BitstreamFormat, EncodedPacket, EncodedStreamInfo, EncoderCapability};
use frame_mux::{AvAudioTrack, MkvAvWriter, interrupted_recordings};
use std::fs;
use std::num::{NonZeroU16, NonZeroU32};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const SPS: [u8; 8] = [0x67, 0x64, 0x00, 0x3C, 0xAC, 0x2B, 0x40, 0x14];
const PPS: [u8; 4] = [0x68, 0xEE, 0x3C, 0xB0];

/// Stereo layout used by the original two-track fixture.
const RATE: u32 = SAMPLE_RATE;
const CHANNELS: u16 = 2;
/// The mixer's ten-millisecond quantum, the largest block `submit` accepts.
const QUANTUM_FRAMES: usize = 480;
/// How many 10 ms quanta this scenario encodes per track: 137 * 10 ms = 1.37 s.
const QUANTA: u64 = 137;
/// Video access units at about 30 fps over the same span.
const VIDEO_FRAMES: u64 = 42;
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
            "frame-mux-av-{label}-{}-{nanos}-{}",
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

/// One video access unit at an absolute presentation timestamp that already includes
/// the session origin.
fn video_packet(pts_ns: u64, index: u64) -> EncodedPacket {
    EncodedPacket {
        track_id: 0,
        pts: MediaTimestamp(pts_ns),
        dts: MediaTimestamp(pts_ns),
        duration_ns: VIDEO_FRAME_NS,
        keyframe: index.is_multiple_of(30),
        data: if index.is_multiple_of(30) {
            let mut unit = Vec::new();
            unit.extend_from_slice(&[0, 0, 0, 1]);
            unit.extend_from_slice(&SPS);
            unit.extend_from_slice(&[0, 0, 0, 1]);
            unit.extend_from_slice(&PPS);
            unit.extend_from_slice(&[0, 0, 0, 1, 0x65]);
            unit.extend_from_slice(&[0xAA; 64]);
            unit
        } else {
            vec![0, 0, 0, 1, 0x41, 0x11, 0x22, 0x33]
        },
    }
}

/// `EncodedPacket` is not `Clone`, so a repeated scenario copies its fields instead.
fn encodable(packet: &EncodedPacket) -> EncodedPacket {
    EncodedPacket {
        track_id: packet.track_id,
        pts: packet.pts,
        dts: packet.dts,
        duration_ns: packet.duration_ns,
        keyframe: packet.keyframe,
        data: packet.data.clone(),
    }
}

/// Two clearly different synthetic stereo tones: 440 Hz on the main mix and 1 kHz on
/// the clean stem, so a decoded stream's content identifies its track.
fn pcm_quantum(track: usize, start_frame: u64, origin_ns: u64) -> frame_audio::TrackPcm {
    let frequency = if track == 0 { 440.0 } else { 1_000.0 };
    let mut samples = Vec::with_capacity(QUANTUM_FRAMES * CHANNELS as usize);
    for offset in 0..QUANTUM_FRAMES as u64 {
        let frame = start_frame + offset;
        let phase = 2.0 * std::f64::consts::PI * frequency * frame as f64 / f64::from(RATE);
        let value = (phase.sin() * 0.5) as f32;
        samples.push(value);
        samples.push(value);
    }
    frame_audio::TrackPcm {
        track_index: track,
        timestamp: MediaTimestamp(origin_ns + start_frame * 1_000_000_000 / u64::from(RATE)),
        sample_rate: NonZeroU32::new(RATE).expect("non-zero"),
        channels: NonZeroU16::new(CHANNELS).expect("non-zero"),
        samples,
    }
}

/// One fully encoded track plus the metadata the container needs.
struct EncodedTrack {
    packets: Vec<EncodedPacket>,
    pre_skip: u16,
    discard_padding: u16,
    /// The encoder's accepted input, asserted against the packet count.
    input_frames: u64,
}

/// Encodes the track's whole synthetic input before any container exists, so the
/// interleave below can deliver packets in timestamp order without reordering them.
fn encode_track(track: usize, origin_ns: u64) -> EncodedTrack {
    let mut encoder =
        OpusTrackEncoder::new(track, MediaTimestamp(origin_ns)).expect("a real Opus encoder");
    let pre_skip = encoder.pre_skip();
    let mut packets = Vec::new();
    for quantum in 0..QUANTA {
        let block = pcm_quantum(track, quantum * QUANTUM_FRAMES as u64, origin_ns);
        if let Some(packet) = encoder
            .submit(&block)
            .expect("the encoder accepts aligned PCM")
        {
            packets.push(packet);
        }
    }
    let finish = encoder.finish().expect("the encoder flushes");
    packets.extend(finish.packets);
    assert_eq!(
        packets.len() as u64,
        (QUANTA * QUANTUM_FRAMES as u64 + u64::from(pre_skip)).div_ceil(FRAME_SAMPLES as u64),
        "the packet count covers the input plus the real lookahead"
    );
    EncodedTrack {
        packets,
        pre_skip: finish.pre_skip,
        discard_padding: finish.discard_padding,
        input_frames: finish.input_frames,
    }
}

/// Merges the video timeline with each track's packets into the non-decreasing
/// delivery order the writer requires, without sorting any packet afterwards.
fn interleave(
    video_frames: u64,
    origin_ns: u64,
    tracks: &mut [EncodedTrack],
) -> Vec<(usize, EncodedPacket)> {
    let mut audio: Vec<std::vec::IntoIter<EncodedPacket>> = tracks
        .iter_mut()
        .map(|track| std::mem::take(&mut track.packets).into_iter())
        .collect();
    let mut sequence = Vec::new();
    let mut frame = 0u64;
    loop {
        let video_pts = (frame < video_frames).then(|| origin_ns + frame * VIDEO_FRAME_NS);
        let audio_pts = audio
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
            sequence.push((
                usize::MAX,
                video_packet(origin_ns + frame * VIDEO_FRAME_NS, frame),
            ));
            frame += 1;
            continue;
        }
        let index = audio
            .iter_mut()
            .position(|queue| {
                queue
                    .as_slice()
                    .first()
                    .is_some_and(|packet| packet.pts.0 == next)
            })
            .expect("a queue holds the earliest audio timestamp");
        let packet = audio[index].next().expect("front exists");
        sequence.push((index, packet));
    }
    assert_eq!(frame, video_frames, "every video frame is delivered");
    assert!(
        audio.iter_mut().all(|queue| queue.as_slice().is_empty()),
        "every audio packet is delivered"
    );
    sequence
}

/// Writes the whole scenario and returns the path plus the writer statistics.
fn write_scenario(
    dir: &Path,
    name: &str,
    origin_ns: u64,
) -> (PathBuf, frame_mux::MkvAvWriterStats) {
    let path = dir.join(name);
    let mut encoded = [encode_track(0, origin_ns), encode_track(1, origin_ns)];
    // Each track's real pre-skip reaches the container header, and both tracks
    // accepted exactly the same number of input frames.
    assert_eq!(encoded[0].input_frames, encoded[1].input_frames);
    assert_eq!(encoded[0].input_frames, QUANTA * QUANTUM_FRAMES as u64);
    let plans = [
        TrackPlan {
            kind: TrackKind::MainMix,
            sources: vec![AudioSourceId("mix".into())],
        },
        TrackPlan {
            kind: TrackKind::Clean(AudioSourceId("stem".into())),
            sources: vec![AudioSourceId("stem".into())],
        },
    ];
    let tracks: Vec<AvAudioTrack> = plans
        .iter()
        .zip(encoded.iter())
        .map(|(plan, track)| AvAudioTrack::from_plan(plan, track.pre_skip).expect("named track"))
        .collect();
    let mut writer = MkvAvWriter::create(&path, &stream_info(), &tracks, MediaTimestamp(origin_ns))
        .expect("a new container");

    for (index, packet) in interleave(VIDEO_FRAMES, origin_ns, &mut encoded) {
        if index == usize::MAX {
            writer.write_video(packet).expect("video packet");
        } else {
            writer.write_audio(packet).expect("audio packet");
        }
    }
    for (index, track) in encoded.iter().enumerate() {
        writer
            .finish_audio(index, track.discard_padding)
            .expect("the track ends with its own padding");
    }
    let stats = writer.stats();
    let artifact = writer.finalize().expect("finalize");
    assert_eq!(artifact.audio_tracks, 2);
    assert_eq!(artifact.path, path.to_string_lossy());
    // The real end is the trimmed end, never the padded container end.
    assert!(
        artifact.duration_ms * 1_000_000.0 >= (QUANTA * 10_000_000) as f64 - 1.0,
        "the artifact spans the encoded audio: {} ms",
        artifact.duration_ms
    );
    assert!(
        stats.trimmed_end_ns <= stats.last_end_ns,
        "trimming may only shorten the last packet"
    );
    (path, stats)
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

fn find(data: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= data.len() {
        return None;
    }
    data[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|position| position + from)
}

fn count(data: &[u8], needle: &[u8]) -> usize {
    let mut total = 0;
    let mut offset = 0;
    while let Some(position) = find(data, needle, offset) {
        total += 1;
        offset = position + 1;
    }
    total
}

/// The decoded picture order of a track's block groups, used to assert the video
/// dependency marks survived the merge with audio.
fn video_dependency_flags(data: &[u8]) -> Vec<bool> {
    let roots = mkwriter::reader::children(data);
    let segment = roots
        .iter()
        .find(|element| element.id == 0x18538067)
        .expect("a segment");
    mkwriter::reader::children(segment.body)
        .into_iter()
        .filter(|element| element.id == 0x1F43B675)
        .flat_map(|cluster| mkwriter::reader::children(cluster.body))
        .filter(|element| element.id == 0xA0)
        .filter(|group| {
            // Video block groups are the ones whose Block carries track number 1.
            mkwriter::reader::children(group.body)
                .iter()
                .find(|element| element.id == 0xA1)
                .and_then(|block| read_vint(block.body, 0).map(|(track, _)| track == 1))
                .unwrap_or(false)
        })
        .map(|group| {
            mkwriter::reader::children(group.body)
                .iter()
                .any(|element| element.id == 0xFB)
        })
        .collect()
}

/// The container requires audio, so a declared audio track must end with a packet
/// count that matches the encoder's own report.
#[test]
fn a_real_av_file_holds_both_opus_tracks_and_the_video_timeline() {
    let scratch = Scratch::new("av");
    let origin = 1_234_567_890u64;
    let (path, stats) = write_scenario(&scratch.dir, "av.mkv", origin);
    let data = fs::read(&path).expect("read back");

    assert_eq!(&data[..4], &[0x1A, 0x45, 0xDF, 0xA3], "EBML magic");
    assert!(
        data.windows(8).any(|window| window == b"matroska"),
        "the file declares the matroska DocType"
    );
    assert_eq!(
        count(&data, b"V_MPEG4/ISO/AVC"),
        1,
        "exactly one H.264 track"
    );
    assert_eq!(count(&data, b"A_OPUS"), 2, "exactly two Opus tracks");
    assert_eq!(count(&data, b"OpusHead"), 2, "each Opus track has a header");
    assert_eq!(count(&data, b"main"), 1, "the main mix is named once");
    assert_eq!(count(&data, b"clean-1"), 1, "the clean stem is named once");
    assert_eq!(stats.audio_tracks, 2);
    assert_eq!(stats.video_packets, VIDEO_FRAMES);
    assert_eq!(stats.audio_finished_tracks, 2);
    assert!(stats.audio_packets > 0);

    // Track numbers: video 1, audio 2 and 3.
    let roots = mkwriter::reader::children(&data);
    let segment = roots
        .iter()
        .find(|element| element.id == 0x18538067)
        .expect("a segment");
    let tracks = mkwriter::reader::children(segment.body)
        .into_iter()
        .find(|element| element.id == 0x1654AE6B)
        .expect("a tracks element");
    let numbers: Vec<u64> = mkwriter::reader::children(tracks.body)
        .iter()
        .map(|entry| {
            mkwriter::reader::children(entry.body)
                .iter()
                .find(|element| element.id == 0xD7)
                .map(|element| mkwriter::reader::as_uint(element.body))
                .expect("a track number")
        })
        .collect();
    assert_eq!(numbers, [1, 2, 3], "video is 1 and audio is 2 and 3");
    let defaults: Vec<_> = mkwriter::reader::children(tracks.body)
        .iter()
        .skip(1)
        .map(|entry| {
            mkwriter::reader::children(entry.body)
                .iter()
                .find(|e| e.id == 0x88)
                .map(|e| mkwriter::reader::as_uint(e.body))
                .expect("explicit audio FlagDefault")
        })
        .collect();
    assert_eq!(defaults, [1, 0], "only the main mix is default");

    // The video keeps its real variable timeline and its dependency marks.
    assert_eq!(stats.first_video_pts_ns, Some(0));
    let dependencies = video_dependency_flags(&data);
    assert_eq!(dependencies.len() as u64, VIDEO_FRAMES);
    for (index, dependent) in dependencies.iter().enumerate() {
        assert_eq!(
            *dependent,
            index % 30 != 0,
            "frame {index} must carry ReferenceBlock only when it is not a keyframe"
        );
    }

    // Cluster timestamps stay inside the timeline. A checkpoint can reopen a
    // cluster at the same timestamp when another track starts there. The
    // clusters are walked through the real element tree, because a byte scan for the
    // cluster identifier also matches it inside block payloads.
    let clusters: Vec<mkwriter::reader::Element> = mkwriter::reader::children(segment.body)
        .into_iter()
        .filter(|element| element.id == 0x1F43B675)
        .collect();
    let cluster_times: Vec<u64> = clusters
        .iter()
        .map(|cluster| {
            let timestamp = mkwriter::reader::children(cluster.body)
                .into_iter()
                .find(|element| element.id == 0xE7)
                .expect("every cluster declares its timestamp");
            mkwriter::reader::as_uint(timestamp.body)
        })
        .collect();
    assert!(cluster_times.len() > 1);
    assert!(cluster_times.windows(2).all(|p| p[0] <= p[1]));
    assert!(
        cluster_times
            .iter()
            .all(|time| *time * 1_000 <= stats.last_end_ns),
        "every cluster lies inside the container timeline"
    );
    let mut block_times: Vec<Vec<u64>> = Vec::new();
    for cluster in &clusters {
        let base = {
            let timestamp = mkwriter::reader::children(cluster.body)
                .into_iter()
                .find(|element| element.id == 0xE7)
                .expect("every cluster declares its timestamp");
            mkwriter::reader::as_uint(timestamp.body)
        };
        let mut absolute = Vec::new();
        for group in mkwriter::reader::children(cluster.body)
            .into_iter()
            .filter(|element| element.id == 0xA0)
        {
            let block = mkwriter::reader::children(group.body)
                .into_iter()
                .find(|element| element.id == 0xA1)
                .expect("a block group holds its block");
            let (track, after_track) = read_vint(block.body, 0).expect("a track number");
            let relative =
                i16::from_be_bytes([block.body[after_track], block.body[after_track + 1]]);
            assert!((1..=3).contains(&track), "block track {track} is declared");
            absolute.push((base as i64 + i64::from(relative)) as u64);
        }
        assert!(
            absolute.windows(2).all(|pair| pair[0] <= pair[1]),
            "cluster at {base} holds out-of-order blocks: {absolute:?}"
        );
        block_times.push(absolute);
    }
    let all_times: Vec<_> = block_times.iter().flatten().copied().collect();
    assert!(
        all_times.windows(2).all(|p| p[0] <= p[1]),
        "on-disk packet order never goes backwards"
    );
    let first_video = block_times
        .iter()
        .flatten()
        .copied()
        .min()
        .expect("the video timeline starts");
    assert_eq!(first_video, 0, "the container starts at the shared origin");

    // Only the last packet of each Opus track carries nonzero DiscardPadding. The element is
    // checked through the tree so a matching byte pair inside a block cannot count.
    let discard_padding = clusters
        .iter()
        .flat_map(|cluster| mkwriter::reader::children(cluster.body))
        .filter(|element| element.id == 0xA0)
        .filter(|group| {
            mkwriter::reader::children(group.body)
                .iter()
                .any(|element| element.id == 0x75A2 && element.body.iter().any(|b| *b != 0))
        })
        .count();
    assert_eq!(
        discard_padding, 2,
        "two nonzero DiscardPadding values, one per audio track"
    );
}

#[test]
fn an_existing_recording_is_never_overwritten() {
    let scratch = Scratch::new("overwrite");
    let path = scratch.path("keep.mkv");
    let original = b"an existing recording";
    fs::write(&path, original).expect("seed file");
    let track = AvAudioTrack::from_plan(&main_plan(), 312).expect("named track");
    let error = MkvAvWriter::create(&path, &stream_info(), &[track], MediaTimestamp(0))
        .expect_err("an existing path is refused");
    assert_eq!(error.code, ErrorCode::InvalidOutputPath);
    assert!(error.message.contains("keep.mkv"));
    assert_eq!(
        fs::read(&path).expect("read back"),
        original,
        "the refused create leaves the existing bytes untouched"
    );
}

#[test]
fn many_mono_and_stereo_tracks_preserve_headers_through_recovery() {
    let scratch = Scratch::new("many-layouts");
    let path = scratch.path("many.mkv");
    let origin = MediaTimestamp(1_000_000_000);
    let mut encoders = Vec::new();
    let mut tracks = Vec::new();
    for index in 0..24 {
        let channels = NonZeroU16::new(if index % 2 == 0 { 1 } else { 2 }).unwrap();
        let encoder = OpusTrackEncoder::with_channels(index, origin, channels).unwrap();
        let id = AudioSourceId(format!("source-{index:02}"));
        let plan = TrackPlan {
            kind: TrackKind::Clean(id.clone()),
            sources: vec![id],
        };
        tracks.push(
            AvAudioTrack::from_plan(&plan, encoder.pre_skip())
                .unwrap()
                .with_channels(channels)
                .unwrap(),
        );
        encoders.push((encoder, channels));
    }
    let mut writer = MkvAvWriter::create(&path, &stream_info(), &tracks, origin).unwrap();
    writer.write_video(video_packet(origin.0, 0)).unwrap();
    for quantum in 0..2 {
        for (index, (encoder, channels)) in encoders.iter_mut().enumerate() {
            let pcm = frame_audio::TrackPcm {
                track_index: index,
                timestamp: MediaTimestamp(origin.0 + quantum * 10_000_000),
                sample_rate: NonZeroU32::new(RATE).unwrap(),
                channels: *channels,
                samples: vec![0.125; 480 * usize::from(channels.get())],
            };
            if let Some(packet) = encoder.submit(&pcm).unwrap() {
                writer.write_audio(packet).unwrap();
            }
        }
    }
    for (index, (encoder, _)) in encoders.iter_mut().enumerate() {
        let finish = encoder.finish().unwrap();
        for packet in finish.packets {
            writer.write_audio(packet).unwrap();
        }
        writer.finish_audio(index, finish.discard_padding).unwrap();
    }
    assert_eq!(writer.finalize().unwrap().audio_tracks, 24);
    let recovered = scratch.path("recovered.mkv");
    let summary = frame_mux::recover(&path, &recovered).unwrap();
    assert_eq!(summary.tracks.len(), 25);
    assert_eq!(summary.recovered_video_packets, 1);
    assert!(
        summary
            .tracks
            .iter()
            .skip(1)
            .all(|track| track.packets == 2)
    );
    fn header(path: &Path) -> Vec<u8> {
        let bytes = fs::read(path).unwrap();
        let segment = mkwriter::reader::children(&bytes)
            .into_iter()
            .find(|e| e.id == 0x18538067)
            .unwrap();
        mkwriter::reader::children(segment.body)
            .into_iter()
            .find(|e| e.id == 0x1654AE6B)
            .unwrap()
            .body
            .to_vec()
    }
    let original_header = header(&path);
    assert_eq!(
        original_header,
        header(&recovered),
        "recovery retains numbers, UIDs, layouts and Opus delay verbatim"
    );
    for (index, entry) in mkwriter::reader::children(&original_header)
        .into_iter()
        .skip(1)
        .enumerate()
    {
        let fields = mkwriter::reader::children(entry.body);
        let private = fields.iter().find(|e| e.id == 0x63A2).unwrap();
        let channels = if index % 2 == 0 { 1 } else { 2 };
        assert_eq!(private.body[9], channels);
        let audio = fields.iter().find(|e| e.id == 0xE1).unwrap();
        let channel_element = mkwriter::reader::children(audio.body)
            .into_iter()
            .find(|e| e.id == 0x9F)
            .unwrap();
        assert_eq!(
            mkwriter::reader::as_uint(channel_element.body),
            u64::from(channels)
        );
    }
}

#[test]
fn a_missing_directory_is_reported_as_an_output_path_error() {
    let scratch = Scratch::new("missing-directory");
    let path = scratch.path("absent").join("out.mkv");
    let error = MkvAvWriter::create(&path, &stream_info(), &[], MediaTimestamp(0))
        .expect_err("the parent directory does not exist");
    assert_eq!(error.code, ErrorCode::InvalidOutputPath);
}

#[test]
fn an_unfinished_or_empty_audio_track_never_produces_an_artifact() {
    let scratch = Scratch::new("incomplete");
    let path = scratch.path("incomplete.mkv");
    let encoded = encode_track(0, 0);
    let tracks = [AvAudioTrack::from_plan(&main_plan(), encoded.pre_skip).expect("named")];
    let mut writer =
        MkvAvWriter::create(&path, &stream_info(), &tracks, MediaTimestamp(0)).expect("create");
    writer.write_video(video_packet(0, 0)).expect("video");
    for packet in encoded.packets.iter().take(4) {
        writer.write_audio(encodable(packet)).expect("audio");
    }
    let error = writer
        .finalize()
        .expect_err("an unfinished audio track has no real end");
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(error.message.contains("never finished"));
    assert!(
        fs::metadata(&path).is_ok_and(|metadata| metadata.len() > 0),
        "the partial file stays for diagnosis and is never reported as an artifact"
    );
    assert_eq!(
        interrupted_recordings(&scratch.dir).unwrap().0,
        vec![path.clone()]
    );

    // An empty declared track is not a successful audio recording either.
    let path = scratch.path("empty.mkv");
    let tracks = [
        AvAudioTrack::from_plan(&main_plan(), 312).expect("named"),
        AvAudioTrack::from_plan(
            &TrackPlan {
                kind: TrackKind::Clean(AudioSourceId("stem".into())),
                sources: vec![AudioSourceId("stem".into())],
            },
            312,
        )
        .expect("named"),
    ];
    let mut writer =
        MkvAvWriter::create(&path, &stream_info(), &tracks, MediaTimestamp(0)).expect("create");
    writer.write_video(video_packet(0, 0)).expect("video");
    for packet in encoded.packets.iter().take(4) {
        writer.write_audio(encodable(packet)).expect("main audio");
    }
    writer
        .finish_audio(0, encoded.discard_padding)
        .expect("main");
    let error = writer
        .finalize()
        .expect_err("the second audio track received nothing");
    assert!(
        error.message.contains("received no packet"),
        "{}",
        error.message
    );
}

fn main_plan() -> TrackPlan {
    TrackPlan {
        kind: TrackKind::MainMix,
        sources: vec![AudioSourceId("mix".into())],
    }
}

#[test]
fn the_complete_scenario_is_written_inside_its_owned_directory() {
    let scratch = Scratch::new("complete-scenario");
    let output = scratch.path("complete.mkv");
    let (path, stats) = write_scenario(&scratch.dir, "complete.mkv", 0);
    assert_eq!(path, output);
    assert!(fs::metadata(&path).expect("metadata").len() > 0);
    assert!(stats.audio_packets > 0);
}
