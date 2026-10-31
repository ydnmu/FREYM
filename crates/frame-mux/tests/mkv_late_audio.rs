// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! A late audio delivery exercises signed block times, backward cluster rollover,
//! Opus final trim, cues, recovery and real decoder seeking without live capture.

use frame_audio::{TrackKind, TrackPcm, TrackPlan};
use frame_clock::MediaTimestamp;
use frame_contracts::AudioSourceId;
use frame_encode::opus::OpusTrackEncoder;
use frame_encode::{BitstreamFormat, EncodedPacket, EncodedStreamInfo, EncoderCapability};
use frame_mux::{AvAudioTrack, MkvAvWriter, recover};
use std::fs;
use std::num::{NonZeroU16, NonZeroU32};
use std::path::Path;
use std::process::Command;

const FRAMES: usize = 60;
// Video ends at 2 s; the static-screen period still delivers audio through 3 s.
const QUANTA: u64 = 300;

fn nals(bytes: &[u8]) -> Vec<(usize, usize, u8)> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 4 < bytes.len() {
        let length = if bytes[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if bytes[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            0
        };
        if length == 0 {
            i += 1;
        } else {
            starts.push((i, length));
            i += length;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(index, (start, prefix))| {
            let end = starts.get(index + 1).map_or(bytes.len(), |next| next.0);
            (*start, end, bytes[start + prefix] & 31)
        })
        .collect()
}

fn video_fixture(raw: &Path) -> (EncodedStreamInfo, Vec<EncodedPacket>) {
    let result = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=128x72:rate=30",
            "-frames:v",
            "60",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-bf",
            "0",
            "-g",
            "30",
            "-keyint_min",
            "30",
            "-sc_threshold",
            "0",
            "-x264-params",
            "aud=1:repeat-headers=1",
            "-f",
            "h264",
            "-y",
        ])
        .arg(raw)
        .output()
        .expect("ffmpeg generates synthetic video");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let bytes = fs::read(raw).expect("generated H.264");
    let ranges = nals(&bytes);
    let access_units: Vec<usize> = ranges
        .iter()
        .filter(|(_, _, kind)| *kind == 9)
        .map(|(start, _, _)| *start)
        .collect();
    assert_eq!(access_units.len(), FRAMES);
    let mut config = Vec::new();
    for (start, end, kind) in &ranges {
        if *kind == 7 || *kind == 8 {
            config.extend_from_slice(&bytes[*start..*end]);
        }
        if *kind == 5 {
            break;
        }
    }
    let stream = EncodedStreamInfo {
        capability: EncoderCapability {
            id: "synthetic-x264".into(),
            codec: "h264".into(),
            hardware: false,
        },
        width: NonZeroU32::new(128).unwrap(),
        height: NonZeroU32::new(72).unwrap(),
        nominal_rate_num: NonZeroU32::new(30).unwrap(),
        nominal_rate_den: NonZeroU32::new(1).unwrap(),
        bitstream_format: BitstreamFormat::H264AnnexB,
        colour: None,
        codec_config: config,
        allows_reordering: false,
    };
    let packets = access_units
        .iter()
        .enumerate()
        .map(|(index, start)| {
            let end = access_units.get(index + 1).copied().unwrap_or(bytes.len());
            let pts = index as u64 * 1_000_000_000 / 30;
            let next = (index as u64 + 1) * 1_000_000_000 / 30;
            let keyframe = nals(&bytes[*start..end])
                .iter()
                .any(|(_, _, kind)| *kind == 5);
            EncodedPacket {
                track_id: 0,
                pts: MediaTimestamp(pts),
                dts: MediaTimestamp(pts),
                duration_ns: next - pts,
                keyframe,
                data: bytes[*start..end].to_vec(),
            }
        })
        .collect();
    (stream, packets)
}

fn audio_fixture() -> (u16, u16, Vec<EncodedPacket>) {
    let mut encoder = OpusTrackEncoder::new(0, MediaTimestamp(0)).unwrap();
    let pre_skip = encoder.pre_skip();
    let mut packets = Vec::new();
    for quantum in 0..QUANTA {
        let mut samples = Vec::with_capacity(960);
        for offset in 0..480 {
            let frame = quantum * 480 + offset;
            let phase = 2.0 * std::f64::consts::PI * 440.0 * frame as f64 / 48_000.0;
            let value = (phase.sin() * 0.3) as f32;
            samples.extend_from_slice(&[value, value]);
        }
        let block = TrackPcm {
            track_index: 0,
            timestamp: MediaTimestamp(quantum * 10_000_000),
            sample_rate: NonZeroU32::new(48_000).unwrap(),
            channels: NonZeroU16::new(2).unwrap(),
            samples,
        };
        if let Some(packet) = encoder.submit(&block).unwrap() {
            packets.push(packet);
        }
    }
    let finish = encoder.finish().unwrap();
    packets.extend(finish.packets);
    (pre_skip, finish.discard_padding, packets)
}

fn copy_packet(packet: &EncodedPacket) -> EncodedPacket {
    EncodedPacket {
        track_id: packet.track_id,
        pts: packet.pts,
        dts: packet.dts,
        duration_ns: packet.duration_ns,
        keyframe: packet.keyframe,
        data: packet.data.clone(),
    }
}

/// Control file: identical encoded packets and trim, delivered in timestamp order.
fn write_ordered_baseline(
    path: &Path,
    stream: &EncodedStreamInfo,
    track: &AvAudioTrack,
    video: &[EncodedPacket],
    audio: &[EncodedPacket],
    padding: u16,
) {
    let mut writer =
        MkvAvWriter::create_live(path, stream, std::slice::from_ref(track), MediaTimestamp(0))
            .expect("ordered control writer");
    let mut sequence: Vec<(u64, bool, usize)> = video
        .iter()
        .enumerate()
        .map(|(index, packet)| (packet.pts.0, true, index))
        .chain(
            audio
                .iter()
                .enumerate()
                .map(|(index, packet)| (packet.pts.0, false, index)),
        )
        .collect();
    sequence.sort_by_key(|(pts, is_video, _)| (*pts, !is_video));
    for (_, is_video, index) in sequence {
        if is_video {
            writer.write_video(copy_packet(&video[index])).unwrap();
        } else {
            writer.write_audio(copy_packet(&audio[index])).unwrap();
        }
    }
    writer.finish_video(1_000_000_000 / 30).unwrap();
    writer.finish_audio(0, padding).unwrap();
    assert_eq!(writer.stats().trimmed_end_ns, 3_000_000_000);
    writer.finalize().unwrap();
}

fn run_ffmpeg(args: &[&str], path: &Path) {
    let mut command = Command::new("ffmpeg");
    command.args(["-v", "error", "-xerror", "-err_detect", "explode"]);
    let result = command
        .arg("-i")
        .arg(path)
        .args(args)
        .args(["-f", "null", "-"])
        .output()
        .expect("ffmpeg decoder");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn seek_frames(path: &Path, timestamp: &str, track: &str, expected_at_least: usize) {
    let frame_limit = if track.contains(":v:") {
        "-frames:v"
    } else {
        "-frames:a"
    };
    let result = Command::new("ffmpeg")
        .args(["-v", "error", "-xerror", "-ss", timestamp, "-i"])
        .arg(path)
        .args(["-map", track, frame_limit, "4", "-f", "framemd5", "-"])
        .output()
        .expect("ffmpeg seek and decode");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let decoded = String::from_utf8(result.stdout).unwrap();
    let frames = decoded
        .lines()
        .filter(|line| !line.starts_with('#'))
        .count();
    assert!(
        frames >= expected_at_least,
        "seek at {timestamp} decoded only {frames} frames"
    );
}

fn audio_framemd5(path: &Path, input_seek: bool) -> Vec<String> {
    let mut command = Command::new("ffmpeg");
    command.args(["-v", "error", "-xerror"]);
    if input_seek {
        command.args(["-ss", "0.12"]);
    }
    command.arg("-i").arg(path);
    if !input_seek {
        command.args(["-ss", "0.12"]);
    }
    let result = command
        .args(["-map", "0:a:0", "-c:a", "pcm_f32le", "-f", "framemd5", "-"])
        .output()
        .expect("ffmpeg audio seek/content comparison");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout)
        .unwrap()
        .lines()
        .filter(|line| !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

type BlockTimes = (Vec<u64>, Vec<(u64, u64, i16)>, Vec<u64>);

fn block_times(path: &Path) -> BlockTimes {
    let data = fs::read(path).unwrap();
    let segment = mkwriter::reader::children(&data)
        .into_iter()
        .find(|element| element.id == 0x18538067)
        .unwrap();
    let mut bases = Vec::new();
    let mut blocks = Vec::new();
    let mut cues = Vec::new();
    for cue_table in mkwriter::reader::children(segment.body)
        .into_iter()
        .filter(|element| element.id == 0x1C53BB6B)
    {
        for point in mkwriter::reader::children(cue_table.body)
            .into_iter()
            .filter(|element| element.id == 0xBB)
        {
            let time = mkwriter::reader::children(point.body)
                .into_iter()
                .find(|element| element.id == 0xB3)
                .unwrap();
            cues.push(mkwriter::reader::as_uint(time.body));
        }
    }
    for cluster in mkwriter::reader::children(segment.body)
        .into_iter()
        .filter(|element| element.id == 0x1F43B675)
    {
        let children = mkwriter::reader::children(cluster.body);
        let base = mkwriter::reader::as_uint(
            children
                .iter()
                .find(|element| element.id == 0xE7)
                .unwrap()
                .body,
        );
        bases.push(base);
        for group in children.iter().filter(|element| element.id == 0xA0) {
            let block = mkwriter::reader::children(group.body)
                .into_iter()
                .find(|element| element.id == 0xA1)
                .unwrap();
            let track = u64::from(block.body[0] & 0x7f);
            let relative = i16::from_be_bytes([block.body[1], block.body[2]]);
            let pts = (base as i64 + i64::from(relative)) as u64;
            blocks.push((track, pts, relative));
        }
    }
    (bases, blocks, cues)
}

#[test]
fn late_audio_keeps_pts_and_decodes_seeks_and_recovers() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-results");
    fs::create_dir_all(&dir).unwrap();
    let raw = dir.join("frame-1104-synthetic.h264");
    let output = dir.join("frame-1104-late.mkv");
    let recovered = dir.join("frame-1104-late-recovered.mkv");
    let baseline = dir.join("frame-1104-ordered.mkv");
    for path in [&raw, &output, &recovered, &baseline] {
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }
    let (stream, video) = video_fixture(&raw);
    let (pre_skip, padding, audio) = audio_fixture();
    let expected_audio_packets = audio.len() as u64;
    let plan = TrackPlan {
        kind: TrackKind::MainMix,
        sources: vec![AudioSourceId("synthetic".into())],
    };
    let track = AvAudioTrack::from_plan(&plan, pre_skip).unwrap();
    write_ordered_baseline(&baseline, &stream, &track, &video, &audio, padding);
    let mut writer =
        MkvAvWriter::create_live(&output, &stream, &[track], MediaTimestamp(0)).unwrap();
    let mut video = video.into_iter();
    let mut audio = audio.into_iter();
    let first_audio = audio.next().unwrap();
    assert_eq!(first_audio.pts.0, 0);
    writer.write_audio(first_audio).unwrap();
    for _ in 0..=30 {
        writer.write_video(video.next().unwrap()).unwrap();
    }
    assert_eq!(writer.stats().video_packets, 31);
    for expected_ms in [20, 40, 60, 80, 100] {
        let packet = audio.next().unwrap();
        assert_eq!(packet.pts.0, expected_ms * 1_000_000);
        writer.write_audio(packet).unwrap();
    }
    for expected_ms in (120..=1_000).step_by(20) {
        let packet = audio.next().unwrap();
        assert_eq!(packet.pts.0, expected_ms * 1_000_000);
        writer.write_audio(packet).unwrap();
    }
    writer.write_video(video.next().unwrap()).unwrap(); // PTS 1033 ms
    let short_late_audio = audio.next().unwrap();
    assert_eq!(short_late_audio.pts.0, 1_020_000_000);
    writer.write_audio(short_late_audio).unwrap();
    for packet in video {
        writer.write_video(packet).unwrap();
    }
    writer.finish_video(1_000_000_000 / 30).unwrap();
    for packet in audio {
        writer.write_audio(packet).unwrap();
    }
    writer.finish_audio(0, padding).unwrap();
    let stats = writer.stats();
    assert_eq!(stats.video_packets, FRAMES as u64);
    assert_eq!(stats.audio_packets, expected_audio_packets);
    assert_eq!(stats.trimmed_end_ns, 3_000_000_000);
    let artifact = writer.finalize().unwrap();
    assert_eq!(artifact.audio_tracks, 1);
    let (bases, blocks, cues) = block_times(&output);
    assert!(
        bases.windows(2).any(|pair| pair[1] < pair[0]),
        "late audio rolls the cluster backward"
    );
    assert!(
        blocks
            .iter()
            .any(|(track, pts, _)| *track == 2 && *pts == 100_000)
    );
    assert!(
        blocks.iter().any(|(_, _, relative)| *relative < 0),
        "short lateness uses signed Block offset"
    );
    let mut video_pts = Vec::new();
    let mut audio_pts = Vec::new();
    for &(track, pts, _) in &blocks {
        match track {
            1 => video_pts.push(pts),
            2 => audio_pts.push(pts),
            _ => panic!("undeclared track"),
        }
    }
    assert!(video_pts.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(audio_pts.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(
        cues,
        [0, 1_000_000],
        "video keyframe cues stay in time order"
    );
    let (_, baseline_blocks, baseline_cues) = block_times(&baseline);
    assert_eq!(baseline_cues, cues);
    let mut late_track_pts: Vec<_> = blocks
        .iter()
        .map(|(track, pts, _)| (*track, *pts))
        .collect();
    let mut baseline_track_pts: Vec<_> = baseline_blocks
        .iter()
        .map(|(track, pts, _)| (*track, *pts))
        .collect();
    late_track_pts.sort_unstable();
    baseline_track_pts.sort_unstable();
    assert_eq!(late_track_pts, baseline_track_pts);
    run_ffmpeg(&["-map", "0:v:0", "-map", "0:a:0"], &output);
    seek_frames(&output, "1.1", "0:v:0", 4);
    let late_audio_seek = audio_framemd5(&output, true);
    let baseline_audio_seek = audio_framemd5(&baseline, true);
    assert!(late_audio_seek.len() >= 4);
    assert_eq!(
        late_audio_seek, baseline_audio_seek,
        "early audio input seek must decode the same timeline and PCM as ordered delivery"
    );
    let late_audio_linear = audio_framemd5(&output, false);
    let baseline_audio_linear = audio_framemd5(&baseline, false);
    assert_eq!(
        late_audio_linear, baseline_audio_linear,
        "linear decode and trim must also match the ordered control"
    );
    let summary = recover(&output, &recovered).unwrap();
    assert_eq!(summary.recovered_video_packets, FRAMES as u64);
    assert_eq!(summary.recovered_audio_packets, stats.audio_packets);
    assert!(summary.final_trim_known);
    assert_eq!(summary.timestamp_scale_ns, 1_000);
    assert_eq!(summary.source_declared_duration_ns, Some(3_000_000_000));
    assert_eq!(summary.duration_ns, 3_000_000_000);
    let (recovered_bases, recovered_blocks, recovered_cues) = block_times(&recovered);
    assert!(recovered_bases.windows(2).any(|pair| pair[1] < pair[0]));
    assert_eq!(
        recovered_blocks.len(),
        (FRAMES as u64 + stats.audio_packets) as usize
    );
    assert_eq!(
        recovered_blocks, blocks,
        "recovery keeps each track PTS and delivery position"
    );
    assert_eq!(recovered_cues, cues);
    assert_eq!(
        fs::read(&recovered).unwrap(),
        fs::read(&output).unwrap(),
        "a finalized late-packet file remuxes byte for byte"
    );
    run_ffmpeg(&["-map", "0:v:0", "-map", "0:a:0"], &recovered);
    seek_frames(&recovered, "1.1", "0:v:0", 4);
    seek_frames(&recovered, "0.12", "0:a:0", 4);
    fs::remove_file(raw).unwrap();
}
