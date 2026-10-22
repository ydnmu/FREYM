//! Focused software fixtures for rolling budgets, atomic retirement and export.
use frame_clock::MediaTimestamp;
use frame_encode::{
    BitstreamFormat, EncodedPacket, EncodedStreamInfo, EncoderCapability, VideoColour,
};
use frame_mux::replay::{DiskReplayLimit, ReplayBuffer, ReplayLimit, RollingReplayStore};
use frame_mux::{SegmentState, read_session_manifest, recover_session};
use std::{num::NonZeroU32, path::PathBuf, process::Command};

fn scratch() -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let root = if let Some(root) = std::env::var_os("FREYM_REPLAY_EVIDENCE") {
        PathBuf::from(root)
    } else {
        std::env::temp_dir().join("freym-rolling-replay")
    };
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join(format!(
        "{}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&path).unwrap();
    path
}
fn stream(rate: u32) -> EncodedStreamInfo {
    EncodedStreamInfo {
        capability: EncoderCapability {
            id: "fixture-main10".into(),
            codec: "hevc".into(),
            hardware: false,
        },
        width: NonZeroU32::new(64).unwrap(),
        height: NonZeroU32::new(64).unwrap(),
        nominal_rate_num: NonZeroU32::new(rate).unwrap(),
        nominal_rate_den: NonZeroU32::new(1).unwrap(),
        bitstream_format: BitstreamFormat::HevcAnnexB,
        colour: Some(VideoColour::BT2020_PQ_LIMITED),
        codec_config: include_bytes!("fixtures/synthetic-main10.hevc").to_vec(),
        allows_reordering: false,
    }
}
fn buffer() -> ReplayBuffer {
    ReplayBuffer::new(
        stream(10),
        vec![],
        ReplayLimit {
            duration_ns: 1_000_000_000,
            max_bytes: 1024 * 1024,
        },
    )
    .unwrap()
}
fn push_second(buffer: &mut ReplayBuffer, second: u64) {
    for index in 0..10 {
        let pts = second * 1_000_000_000 + index * 100_000_000;
        buffer.push_video(&EncodedPacket {
            track_id: 0,
            pts: MediaTimestamp(pts),
            dts: MediaTimestamp(pts),
            duration_ns: 100_000_000,
            keyframe: true,
            data: include_bytes!("fixtures/synthetic-main10.hevc").to_vec(),
        });
    }
}
fn limit(count: usize) -> DiskReplayLimit {
    DiskReplayLimit {
        segment_ns: 1_000_000_000,
        history_ns: 2_000_000_000,
        max_bytes: 16 * 1024 * 1024,
        max_segments: count,
    }
}
fn decode(path: &std::path::Path, output: &std::path::Path) {
    let result = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args([
            "-map",
            "0:v:0",
            "-fps_mode",
            "passthrough",
            "-f",
            "framemd5",
        ])
        .arg(output)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let frames = std::fs::read_to_string(output).unwrap();
    assert_eq!(
        frames
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .count(),
        10
    );
}

#[test]
fn rolling_clips_are_bounded_independent_and_exported_with_frozen_tracks() {
    let root = scratch();
    let mut store = RollingReplayStore::create(&root.join("owned"), limit(2)).unwrap();
    assert!(RollingReplayStore::create(&root.join("owned"), limit(2)).is_err());
    let mut replay = buffer();
    for second in 0..4 {
        push_second(&mut replay, second);
        assert!(store.checkpoint_if_due(&replay).unwrap());
        assert!(
            !store.checkpoint_if_due(&replay).unwrap(),
            "unchanged source cannot generate another clip"
        );
    }
    // Real codec changes are new independent clips, not packets in old Tracks.
    let av1_fixture = include_bytes!("fixtures/synthetic-main10.av1").to_vec();
    let mut av1_stream = stream(10);
    av1_stream.capability.codec = "av1".into();
    av1_stream.capability.id = "fixture-av1-main10".into();
    av1_stream.bitstream_format = BitstreamFormat::Av1Obu;
    // Fixture's temporal delimiter + single short sequence header, no Frame OBU.
    let config_end = 4 + usize::from(av1_fixture[3]);
    av1_stream.codec_config = av1_fixture[..config_end].to_vec();
    replay.reset_stream(av1_stream, vec![]).unwrap();
    for index in 0..10 {
        let pts = 4_000_000_000 + index * 100_000_000;
        replay.push_video(&EncodedPacket {
            track_id: 0,
            pts: MediaTimestamp(pts),
            dts: MediaTimestamp(pts),
            duration_ns: 100_000_000,
            keyframe: true,
            data: av1_fixture.clone(),
        });
    }
    assert!(store.checkpoint_if_due(&replay).unwrap());
    let manifest = store.snapshot().unwrap();
    assert_eq!(manifest.segments.len(), 2);
    assert!(
        manifest
            .segments
            .iter()
            .all(|s| s.state == SegmentState::Closed)
    );
    assert_ne!(
        manifest.segments[0].tracks_ebml,
        manifest.segments[1].tracks_ebml
    );
    assert_eq!(store.stats().sealed_segments, 5);
    assert_eq!(store.stats().retired_segments, 3);
    assert!(store.stats().disk_bytes <= limit(2).max_bytes);
    assert!(replay.retained_bytes() <= 1024 * 1024);
    let export = root.join("export.freym-session");
    let result = store.export(&export).unwrap();
    assert!(result.manifest.completed);
    assert_eq!(result.verified_closed_segments, 2);
    assert!(result.rejected_segments.is_empty());
    for (index, segment) in result.manifest.segments.iter().enumerate() {
        assert_eq!(segment.tracks_ebml, manifest.segments[index].tracks_ebml);
        let original = root.join("owned").join(&manifest.segments[index].file_name);
        let copied = root.join(&segment.file_name);
        assert_eq!(
            std::fs::read(original).unwrap(),
            std::fs::read(&copied).unwrap()
        );
        decode(&copied, &root.join(format!("clip-{index}.framemd5")));
    }
    // The original live journal blocks arbitrary recovery, while owner export is safe.
    assert!(
        recover_session(
            store.manifest_path(),
            &root.join("live-recovery.freym-session")
        )
        .is_err()
    );
    assert!(
        RollingReplayStore::recover_owned(
            &root.join("owned"),
            &root.join("live-owned-recovery.freym-session")
        )
        .is_err()
    );
    drop(store);
    let recovered = RollingReplayStore::recover_owned(
        &root.join("owned"),
        &root.join("crash-recovery.freym-session"),
    )
    .unwrap();
    assert_eq!(recovered.verified_closed_segments, 2);
    assert!(recovered.manifest.completed);
    for (index, segment) in recovered.manifest.segments.iter().enumerate() {
        let copied = root.join(&segment.file_name);
        assert_eq!(
            std::fs::read(root.join("owned").join(&manifest.segments[index].file_name)).unwrap(),
            std::fs::read(&copied).unwrap()
        );
        decode(
            &copied,
            &root.join(format!("offline-clip-{index}.framemd5")),
        );
    }
    println!("Rolling replay evidence: {}", root.display());
}

#[test]
fn corrupt_retirement_and_failed_atomic_publication_preserve_files_and_stop_rotation() {
    for corrupt in [true, false] {
        let root = scratch();
        let mut replay = buffer();
        let mut store = RollingReplayStore::create(&root.join("owned"), limit(1)).unwrap();
        push_second(&mut replay, 0);
        assert!(store.checkpoint_if_due(&replay).unwrap());
        let original = store.snapshot().unwrap();
        let segment = root.join("owned").join(&original.segments[0].file_name);
        if corrupt {
            std::fs::write(&segment, b"injected corruption").unwrap();
        } else {
            let blocked = PathBuf::from(format!(
                "{}.tmp-{}-{}",
                store.manifest_path().display(),
                std::process::id(),
                original.generation + 1
            ));
            std::fs::create_dir(blocked).unwrap();
        }
        let before = std::fs::read(&segment).unwrap();
        push_second(&mut replay, 1);
        assert!(store.checkpoint_if_due(&replay).is_err());
        assert_eq!(std::fs::read(&segment).unwrap(), before);
        assert_eq!(
            read_session_manifest(store.manifest_path()).unwrap(),
            original
        );
        assert!(
            store.checkpoint_if_due(&replay).is_err(),
            "failure is sticky"
        );
    }
}

#[test]
fn manifest_export_validates_before_creation_and_never_overwrites() {
    let root = scratch();
    let mut replay = buffer();
    let output = root.join("replay.mkv");
    assert!(replay.save_session(&output, 1_000_000_000).is_err());
    assert!(!root.join("replay.mkv.freym-session").exists());
    push_second(&mut replay, 0);
    replay.save_session(&output, 1_000_000_000).unwrap();
    let manifest = read_session_manifest(&root.join("replay.mkv.freym-session")).unwrap();
    assert!(manifest.completed);
    let before = std::fs::read(&output).unwrap();
    assert!(replay.save_session(&output, 1_000_000_000).is_err());
    assert_eq!(std::fs::read(&output).unwrap(), before);
    decode(&output, &root.join("single.framemd5"));
}

#[test]
fn interrupted_export_keeps_a_discoverable_verified_prefix_and_all_source_segments() {
    let root = scratch();
    let mut store = RollingReplayStore::create(&root.join("owned"), limit(2)).unwrap();
    let mut replay = buffer();
    for second in 0..2 {
        push_second(&mut replay, second);
        store.checkpoint_if_due(&replay).unwrap();
    }
    let before = store.snapshot().unwrap();
    let originals: Vec<_> = before
        .segments
        .iter()
        .map(|s| std::fs::read(root.join("owned").join(&s.file_name)).unwrap())
        .collect();
    let output = root.join("interrupted.freym-session");
    // Fail publication after copying the second segment; the first checkpoint
    // must remain durable and the unreferenced copy must never be deleted.
    let blocked = PathBuf::from(format!("{}.tmp-{}-2", output.display(), std::process::id()));
    std::fs::create_dir(&blocked).unwrap();
    assert!(store.export(&output).is_err());
    let partial = read_session_manifest(&output).unwrap();
    assert!(!partial.completed);
    assert_eq!(partial.segments.len(), 1);
    assert!(
        frame_mux::interrupted_recordings(&root)
            .unwrap()
            .0
            .contains(&output)
    );
    assert_eq!(store.snapshot().unwrap(), before);
    for (segment, bytes) in before.segments.iter().zip(originals) {
        assert_eq!(
            std::fs::read(root.join("owned").join(&segment.file_name)).unwrap(),
            bytes
        );
    }
    let recovered = recover_session(&output, &root.join("resumed.freym-session")).unwrap();
    assert_eq!(recovered.verified_closed_segments, 1);
    assert!(recovered.rejected_segments.is_empty());
    decode(
        &root.join(&recovered.manifest.segments[0].file_name),
        &root.join("resumed.framemd5"),
    );
}

#[test]
fn byte_pressure_retires_a_verified_clip_before_writing_the_next() {
    let root = scratch();
    let mut limits = limit(10);
    limits.history_ns = 10_000_000_000;
    let mut store = RollingReplayStore::create(&root.join("owned"), limits).unwrap();
    let mut replay = ReplayBuffer::new(
        stream(1),
        vec![],
        ReplayLimit {
            duration_ns: 1_000_000_000,
            max_bytes: 5 * 1024 * 1024,
        },
    )
    .unwrap();
    let mut payload = include_bytes!("fixtures/synthetic-main10.hevc").to_vec();
    // Valid HEVC filler_data NAL tests storage pressure without native encoding.
    payload.extend_from_slice(&[0, 0, 0, 1, 0x4c, 0x01]);
    payload.resize(4 * 1024 * 1024, 0xff);
    *payload.last_mut().unwrap() = 0x80;
    let mut old_file = None;
    for second in 0..2 {
        let pts = second * 1_000_000_000;
        replay.push_video(&EncodedPacket {
            track_id: 0,
            pts: MediaTimestamp(pts),
            dts: MediaTimestamp(pts),
            duration_ns: 1_000_000_000,
            keyframe: true,
            data: payload.clone(),
        });
        assert!(store.checkpoint_if_due(&replay).unwrap());
        if second == 0 {
            old_file = Some(
                root.join("owned")
                    .join(&store.snapshot().unwrap().segments[0].file_name),
            );
        }
    }
    assert_eq!(store.stats().sealed_segments, 2);
    assert_eq!(store.stats().retired_segments, 1);
    assert_eq!(store.stats().retained_segments, 1);
    assert!(store.stats().disk_bytes <= limits.max_bytes);
    assert!(!old_file.unwrap().exists());
}
