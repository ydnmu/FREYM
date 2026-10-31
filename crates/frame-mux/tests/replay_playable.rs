//! Synthetic encoded media proves bounded replay remux and full decoder playback.
//! The test generator captures no screen or microphone.
use frame_audio::{TrackKind, TrackPcm, TrackPlan};
use frame_clock::{MediaTimestamp, PauseTimeline};
use frame_contracts::AudioSourceId;
use frame_encode::opus::OpusTrackEncoder;
use frame_encode::{BitstreamFormat, EncodedPacket, EncodedStreamInfo, EncoderCapability};
use frame_mux::AvAudioTrack;
use frame_mux::replay::{ReplayBuffer, ReplayLimit};
use std::num::{NonZeroU16, NonZeroU32};
use std::process::Command;

fn nal_ranges(bytes: &[u8]) -> Vec<(usize, usize, u8)> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 4 < bytes.len() {
        let code = if bytes[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if bytes[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            0
        };
        if code > 0 {
            starts.push((i, code));
            i += code;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(index, (start, code))| {
            let end = starts.get(index + 1).map_or(bytes.len(), |next| next.0);
            (*start, end, bytes[start + code] & 31)
        })
        .collect()
}

#[test]
fn bounded_encoded_av_replay_decodes_without_screen_capture() {
    let evidence_root = std::env::var_os("FREYM_REPLAY_EVIDENCE").map(std::path::PathBuf::from);
    let scratch_root = evidence_root.clone().unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&scratch_root).unwrap();
    let scratch = scratch_root.join(format!(
        "frame-0306-replay-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&scratch).unwrap();
    let raw = scratch.join("synthetic.h264");
    let output = scratch.join("replay.mkv");
    let generated = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=128x72:rate=30",
            "-frames:v",
            "90",
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
        .arg(&raw)
        .output()
        .expect("ffmpeg installed for playable replay proof");
    assert!(
        generated.status.success(),
        "{}",
        String::from_utf8_lossy(&generated.stderr)
    );
    let bytes = std::fs::read(&raw).unwrap();
    let nals = nal_ranges(&bytes);
    let aud: Vec<usize> = nals
        .iter()
        .filter(|(_, _, kind)| *kind == 9)
        .map(|(start, _, _)| *start)
        .collect();
    assert_eq!(aud.len(), 90, "one access-unit delimiter per encoded frame");
    let mut config = Vec::new();
    for (start, end, kind) in &nals {
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
    let plan = TrackPlan {
        kind: TrackKind::MainMix,
        sources: vec![AudioSourceId("synthetic".into())],
    };
    let track = AvAudioTrack::from_plan(&plan, 0).unwrap();
    let mut replay = ReplayBuffer::new(
        stream.clone(),
        vec![track],
        ReplayLimit {
            duration_ns: 2_000_000_000,
            max_bytes: 1024 * 1024,
        },
    )
    .unwrap();
    let mut packets: Vec<(EncodedPacket, bool)> = aud
        .iter()
        .enumerate()
        .map(|(index, start)| {
            let end = aud.get(index + 1).copied().unwrap_or(bytes.len());
            let pts = index as u64 * 1_000_000_000 / 30;
            let next = (index as u64 + 1) * 1_000_000_000 / 30;
            let keyframe = nal_ranges(&bytes[*start..end])
                .iter()
                .any(|(_, _, kind)| *kind == 5);
            (
                EncodedPacket {
                    track_id: 0,
                    pts: MediaTimestamp(pts),
                    dts: MediaTimestamp(pts),
                    duration_ns: next - pts,
                    keyframe,
                    data: bytes[*start..end].to_vec(),
                },
                true,
            )
        })
        .collect();
    let mut encoder = OpusTrackEncoder::new(0, MediaTimestamp(0)).unwrap();
    for index in 0..320u64 {
        let pcm = TrackPcm {
            track_index: 0,
            timestamp: MediaTimestamp(index * 10_000_000),
            sample_rate: NonZeroU32::new(48_000).unwrap(),
            channels: NonZeroU16::new(2).unwrap(),
            samples: vec![0.0; 960],
        };
        if let Some(packet) = encoder.submit(&pcm).unwrap() {
            packets.push((packet, false));
        }
    }
    packets.extend(
        encoder
            .finish()
            .unwrap()
            .packets
            .into_iter()
            .map(|packet| (packet, false)),
    );
    packets.sort_by_key(|(packet, video)| (packet.pts.0, !video));
    let mut missing_audio = ReplayBuffer::new(
        stream.clone(),
        vec![AvAudioTrack::from_plan(&plan, 0).unwrap()],
        ReplayLimit {
            duration_ns: 2_000_000_000,
            max_bytes: 1024 * 1024,
        },
    )
    .unwrap();
    let mut missing_keyframe = ReplayBuffer::new(
        stream.clone(),
        Vec::new(),
        ReplayLimit {
            duration_ns: 2_000_000_000,
            max_bytes: 1024 * 1024,
        },
    )
    .unwrap();
    let mut timeline = PauseTimeline::default();
    let mut resumed = false;
    for (packet, video) in &packets {
        if !resumed && packet.pts.0 >= 1_000_000_000 {
            assert_eq!(
                timeline.pause(MediaTimestamp(1_000_000_000)),
                Some(MediaTimestamp(1_000_000_000))
            );
            assert_eq!(
                timeline.resume(MediaTimestamp(6_000_000_000)),
                Some(MediaTimestamp(1_000_000_000))
            );
            resumed = true;
        }
        let raw_pts = if packet.pts.0 >= 1_000_000_000 {
            packet.pts.0 + 5_000_000_000
        } else {
            packet.pts.0
        };
        let mapped = timeline.map(MediaTimestamp(raw_pts)).unwrap();
        assert_eq!(
            mapped, packet.pts,
            "the pause must not add a five-second gap"
        );
        let mut packet = packet.clone();
        packet.pts = mapped;
        packet.dts = mapped;
        if *video {
            replay.push_video(&packet);
            missing_audio.push_video(&packet);
            if !packet.keyframe {
                missing_keyframe.push_video(&packet);
            }
        } else {
            replay.push_audio(&packet);
        }
        assert!(replay.retained_bytes() <= 1024 * 1024);
    }
    assert_eq!(
        timeline.pause(MediaTimestamp(8_200_000_000)),
        Some(MediaTimestamp(3_200_000_000))
    );
    assert_eq!(
        timeline.map(MediaTimestamp(100_000_000_000)),
        Some(MediaTimestamp(3_200_000_000)),
        "Stop while paused must use the frozen media boundary"
    );
    assert!(
        missing_audio
            .save(&scratch.join("missing-audio.mkv"), 1_000_000_000)
            .is_err()
    );
    assert!(
        missing_keyframe
            .save(&scratch.join("missing-keyframe.mkv"), 1_000_000_000)
            .is_err()
    );
    let artifact = replay.save_session(&output, 1_000_000_000).unwrap();
    let manifest =
        frame_mux::read_session_manifest(&scratch.join("replay.mkv.freym-session")).unwrap();
    assert!(manifest.completed);
    assert_eq!(manifest.segments.len(), 1);
    assert_eq!(artifact.audio_tracks, 1);
    assert!(artifact.duration_ms >= 950.0 && artifact.duration_ms <= 1100.0);
    let decoded = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&output)
        .args(["-map", "0:v:0", "-map", "0:a:0", "-f", "null", "-"])
        .output()
        .unwrap();
    assert!(
        decoded.status.success(),
        "{}",
        String::from_utf8_lossy(&decoded.stderr)
    );
    // Snapshot ownership must preserve separate track labels, timing and trim.
    let clean = TrackPlan {
        kind: TrackKind::Clean(AudioSourceId("separate-fixture".into())),
        sources: vec![AudioSourceId("separate-fixture".into())],
    };
    let mut multi = ReplayBuffer::new(
        stream.clone(),
        vec![
            AvAudioTrack::from_plan(&plan, 312).unwrap(),
            AvAudioTrack::from_plan(&clean, 312).unwrap(),
        ],
        ReplayLimit {
            duration_ns: 2_000_000_000,
            max_bytes: 1024 * 1024,
        },
    )
    .unwrap();
    for (packet, video) in &packets {
        if *video {
            multi.push_video(packet);
        } else {
            multi.push_audio(packet);
            let mut separate = packet.clone();
            separate.track_id = 1;
            multi.push_audio(&separate);
        }
    }
    let frozen = multi.clone();
    multi.reset_stream(stream, vec![]).unwrap();
    let multi_path = scratch.join("separate-replay.mkv");
    assert_eq!(
        frozen
            .save_session(&multi_path, 1_000_000_000)
            .unwrap()
            .audio_tracks,
        2
    );
    let info = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name,start_time:stream_tags=title",
            "-of",
            "json",
        ])
        .arg(&multi_path)
        .output()
        .unwrap();
    assert!(info.status.success());
    let info: serde_json::Value = serde_json::from_slice(&info.stdout).unwrap();
    let tracks = info["streams"].as_array().unwrap();
    assert_eq!(tracks.len(), 3);
    assert_eq!(tracks[1]["tags"]["title"], "main");
    assert_eq!(tracks[2]["tags"]["title"], "clean-1");
    assert_eq!(tracks[1]["start_time"], tracks[2]["start_time"]);
    let playback = Command::new("ffmpeg")
        .args(["-v", "error", "-xerror", "-i"])
        .arg(&multi_path)
        .args(["-map", "0", "-f", "null", "-"])
        .output()
        .unwrap();
    assert!(
        playback.status.success(),
        "{}",
        String::from_utf8_lossy(&playback.stderr)
    );
    let mut disk = frame_mux::replay::RollingReplayStore::create(
        &scratch.join("rolling"),
        frame_mux::replay::DiskReplayLimit {
            segment_ns: 1_000_000_000,
            history_ns: 2_000_000_000,
            max_bytes: 16 * 1024 * 1024,
            max_segments: 2,
        },
    )
    .unwrap();
    assert!(disk.checkpoint_if_due(&replay).unwrap());
    let archive = disk
        .export(&scratch.join("rolling-export.freym-session"))
        .unwrap();
    assert_eq!(archive.verified_closed_segments, 1);
    assert!(archive.manifest.completed);
    let av_clip = scratch.join(&archive.manifest.segments[0].file_name);
    let decoded_av = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(av_clip)
        .args(["-map", "0:v:0", "-map", "0:a:0", "-f", "null", "-"])
        .output()
        .unwrap();
    assert!(
        decoded_av.status.success(),
        "{}",
        String::from_utf8_lossy(&decoded_av.stderr)
    );
    drop(disk);
    if evidence_root.is_some() {
        println!("A/V replay evidence: {}", scratch.display());
    } else {
        std::fs::remove_dir_all(&scratch).unwrap();
    }
}

#[test]
fn main10_replay_preserves_hdr_and_refuses_false_random_access_after_restart() {
    let scratch_root = std::env::temp_dir().join("freym-hdr-replay");
    std::fs::create_dir_all(&scratch_root).unwrap();
    let scratch = scratch_root.join(format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&scratch).unwrap();
    let fixture = include_bytes!("fixtures/synthetic-main10.hevc").to_vec();
    let stream = EncodedStreamInfo {
        capability: EncoderCapability {
            id: "fixture-main10".into(),
            codec: "hevc".into(),
            hardware: false,
        },
        width: NonZeroU32::new(64).unwrap(),
        height: NonZeroU32::new(64).unwrap(),
        nominal_rate_num: NonZeroU32::new(480).unwrap(),
        nominal_rate_den: NonZeroU32::new(1).unwrap(),
        bitstream_format: BitstreamFormat::HevcAnnexB,
        colour: Some(frame_encode::VideoColour::BT2020_PQ_LIMITED),
        codec_config: fixture.clone(),
        allows_reordering: false,
    };
    let mut replay = ReplayBuffer::new(
        stream.clone(),
        vec![],
        ReplayLimit {
            duration_ns: 1_000_000_000,
            max_bytes: 1024 * 1024,
        },
    )
    .unwrap();
    // Encoded fixture timing at 480 Hz proves remux arithmetic, not native capture FPS.
    for index in 0..480u64 {
        let pts = index * 1_000_000_000 / 480;
        let end = (index + 1) * 1_000_000_000 / 480;
        replay.push_video(&EncodedPacket {
            track_id: 0,
            pts: MediaTimestamp(pts),
            dts: MediaTimestamp(pts),
            duration_ns: end - pts,
            keyframe: true,
            data: fixture.clone(),
        });
    }
    let output = scratch.join("main10-replay.mkv");
    let artifact = replay.save(&output, 250_000_000).unwrap();
    assert!((artifact.duration_ms - 250.0).abs() <= 1.0);
    let decoded = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&output)
        .args([
            "-map",
            "0:v:0",
            "-fps_mode",
            "passthrough",
            "-f",
            "framemd5",
        ])
        .arg(scratch.join("decoded.framemd5"))
        .output()
        .unwrap();
    assert!(
        decoded.status.success(),
        "{}",
        String::from_utf8_lossy(&decoded.stderr)
    );
    let frames = std::fs::read_to_string(scratch.join("decoded.framemd5")).unwrap();
    assert_eq!(
        frames
            .lines()
            .filter(|line| !line.starts_with('#') && !line.is_empty())
            .count(),
        120
    );
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name,pix_fmt,color_space,color_transfer,color_primaries",
            "-of",
            "json",
        ])
        .arg(&output)
        .output()
        .unwrap();
    assert!(probe.status.success());
    let metadata = String::from_utf8(probe.stdout).unwrap();
    for field in ["hevc", "yuv420p10le", "bt2020nc", "smpte2084", "bt2020"] {
        assert!(metadata.contains(field), "{metadata}");
    }
    std::fs::write(scratch.join("ffprobe.json"), metadata).unwrap();
    // Reset discards all old codec packets and false driver flags cannot recreate a RAP.
    replay.reset_stream(stream, vec![]).unwrap();
    assert_eq!(replay.retained_bytes(), 0);
    replay.push_video(&EncodedPacket {
        track_id: 0,
        pts: MediaTimestamp(2_000_000_000),
        dts: MediaTimestamp(2_000_000_000),
        duration_ns: 250_000_000,
        keyframe: true,
        data: vec![0, 0, 0, 1, 0x02, 0x01, 0x80],
    });
    let invalid_output = scratch.join("false-keyframe.mkv");
    assert!(replay.save(&invalid_output, 100_000_000).is_err());
    assert!(!invalid_output.exists());
    println!("Main10 replay evidence: {}", scratch.display());
}
