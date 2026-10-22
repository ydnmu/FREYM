use chrono::{Local, TimeZone};
use frame_contracts::{
    FrameRateStatus, QueueMetrics, RecorderMetrics, RecorderState, VideoFrameCounters,
};
use frame_runtime::{recording_files::*, recording_health::*};
use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

fn scratch() -> PathBuf {
    let root = std::env::temp_dir().join("freym-recording-advanced");
    let path = root.join(format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn output_templates_expand_once_and_never_overwrite_files_or_session_evidence() {
    let now = Local.with_ymd_and_hms(2026, 10, 14, 12, 34, 56).unwrap();
    let directory = scratch();
    assert_eq!(
        expand_filename("Client_{date}_{time}", now).unwrap(),
        "Client_2026-10-14_12-34-56"
    );
    assert_eq!(
        expand_filename("演示_{timestamp}", now).unwrap(),
        "演示_2026-10-14_12-34-56"
    );
    let first = resolve_output(&directory, "Client_{timestamp}", now).unwrap();
    fs::write(&first, b"keep existing media").unwrap();
    let second = resolve_output(&directory, "Client_{timestamp}", now).unwrap();
    assert!(
        second
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("_2.mkv")
    );
    fs::write(
        second.with_extension("mkv.freym-session"),
        b"keep interrupted manifest",
    )
    .unwrap();
    let third = resolve_output(&directory, "Client_{timestamp}", now).unwrap();
    assert!(
        third
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("_3.mkv")
    );
    fs::write(
        third.with_extension("mkv.freym-session.freym-incomplete"),
        b"keep orphan journal",
    )
    .unwrap();
    let fourth = resolve_output(&directory, "Client_{timestamp}", now).unwrap();
    assert!(
        fourth
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("_4.mkv")
    );
    assert_eq!(fs::read(first).unwrap(), b"keep existing media");
    let exact = resolve_output(&directory, "Exact", now).unwrap();
    fs::write(
        exact.with_extension("mkv.freym-incomplete"),
        b"keep evidence",
    )
    .unwrap();
    assert!(resolve_output(&directory, "Exact", now).is_err());
    assert!(resolve_output(std::path::Path::new("relative"), "Name", now).is_err());
}

#[test]
fn unsafe_filenames_and_unknown_tokens_are_rejected_before_start() {
    for stem in [
        "",
        ".",
        "..",
        "../escape",
        "folder\\escape",
        "CON",
        "nul.txt",
        "COM1",
        "LPT9",
        "LPT¹",
        "trailing.",
        "{unknown}",
        "{date",
        "time}",
        "{{date}}",
        "colon:",
        "a\0b",
    ] {
        assert!(
            validate_filename_template(stem).is_err(),
            "{stem:?} must be refused"
        );
    }
    assert!(validate_filename_template(&"x".repeat(201)).is_err());
    assert!(validate_filename_template("CONSOLE_{date}").is_ok());
    assert!(validate_filename_template("project-{date}-{time}").is_ok());
}

#[test]
fn storage_observations_preserve_zero_unknown_and_threshold_semantics() {
    assert!(storage_warning(Ok(LOW_SPACE_WARNING_BYTES)).is_none());
    let zero = storage_warning(Ok(0)).unwrap();
    assert_eq!(zero.code, "recordingDiskSpaceLow");
    assert!(zero.message.contains("0 bytes"));
    let unknown = storage_warning(Err("volume disconnected".into())).unwrap();
    assert_eq!(unknown.code, "recordingDiskSpaceUnknown");
    assert!(unknown.message.contains("could not be measured"));
}

fn metrics() -> RecorderMetrics {
    RecorderMetrics {
        frame_rate: FrameRateStatus::default(),
        elapsed_ms: 0.0,
        capture_latency_ms: None,
        encode_latency_ms: None,
        mux_latency_ms: None,
        queues: vec![],
        cpu_percent: None,
        gpu_percent: None,
        memory_bytes: None,
        av_offset_ms: None,
        audio_underruns: None,
        disk_bytes_per_second: None,
    }
}

#[test]
fn diagnostics_keep_capture_losses_policy_skips_and_unknowns_distinct() {
    let mut m = metrics();
    m.frame_rate.actual_capture_fps = Some(0.0);
    m.frame_rate.encoded_fps = Some(f64::NAN);
    m.frame_rate.counters = Some(VideoFrameCounters {
        source_presentations: 90,
        captured_frames: 100,
        captured_unique_frames: 80,
        submitted_frames: 75,
        encoded_frames: 74,
        policy_skipped_frames: 5,
        backpressure_events: 17,
        pool_pressure_drops: 2,
        ready_queue_drops: 3,
        contention_drops: 4,
        upstream_accumulated_updates: 200,
    });
    m.frame_rate.dropped_frames = Some(9);
    m.queues.push(QueueMetrics {
        name: "Encoder ingress".into(),
        depth: 3,
        capacity: 3,
        dropped: 0,
    });
    let rows = diagnostics(Some(&m));
    let value = |label| {
        rows.iter()
            .find(|r| r.label == label)
            .unwrap()
            .value
            .as_str()
    };
    assert_eq!(value("Capture delivered"), "0.00 FPS");
    assert_eq!(value("Encoded pictures"), "Unmeasured");
    assert_eq!(value("Measured capture loss total"), "9");
    assert_eq!(value("Intentional FPS policy skips"), "5");
    assert_eq!(value("Encoder backpressure events"), "17");
    assert!(value("Duplicate encoded frames").starts_with("Unmeasured"));
    assert_eq!(value("Encoding latency"), "Unmeasured");
    assert_eq!(value("Hardware encoder utilization"), "Unmeasured");
    assert_eq!(value("Content A/V offset"), "Unmeasured");
    assert!(value("Queue pressure").contains("full at this observation"));
    assert_eq!(diagnostics(None)[0].value, "No session measurements yet");
    m.frame_rate.dropped_frames = None;
    assert_eq!(
        diagnostics(Some(&m))
            .iter()
            .find(|r| r.label == "Measured capture loss total")
            .unwrap()
            .value,
        "Unmeasured"
    );
}

#[test]
fn short_recording_verification_requires_completed_manifest_and_detects_modified_media() {
    use frame_clock::MediaTimestamp;
    use frame_encode::{
        BitstreamFormat, EncodedPacket, EncodedStreamInfo, EncoderCapability, VideoColour,
    };
    use frame_mux::{ContainerWriter, RecordingSession};
    use std::num::NonZeroU32;
    let directory = scratch();
    let output = directory.join("short.mkv");
    let index = output.with_extension("mkv.freym-session");
    let fixture = include_bytes!("../../frame-mux/tests/fixtures/synthetic-main10.hevc");
    let stream = EncodedStreamInfo {
        capability: EncoderCapability {
            id: "test-fixture".into(),
            codec: "hevc".into(),
            hardware: false,
        },
        width: NonZeroU32::new(64).unwrap(),
        height: NonZeroU32::new(64).unwrap(),
        nominal_rate_num: NonZeroU32::new(1).unwrap(),
        nominal_rate_den: NonZeroU32::new(1).unwrap(),
        bitstream_format: BitstreamFormat::HevcAnnexB,
        colour: Some(VideoColour::BT2020_PQ_LIMITED),
        codec_config: fixture.to_vec(),
        allows_reordering: false,
    };
    let mut session = RecordingSession::create(&index).unwrap();
    let mut writer = session
        .create_video_segment(&stream, MediaTimestamp(0))
        .unwrap();
    writer
        .write(EncodedPacket {
            track_id: 1,
            pts: MediaTimestamp(0),
            dts: MediaTimestamp(0),
            duration_ns: 1_000_000_000,
            keyframe: true,
            data: fixture.to_vec(),
        })
        .unwrap();
    assert!(verify_finalized(&output).is_err());
    writer.finalize().unwrap();
    assert!(
        verify_finalized(&output).is_err(),
        "A closed segment alone does not complete its session"
    );
    let manifest = session.finish().unwrap();
    let file = directory.join(&manifest.segments[0].file_name);
    let before = fs::read(&file).unwrap();
    let verified = verify_finalized(&output).unwrap();
    assert_eq!(verified.segments, 1);
    assert_eq!(verified.bytes, before.len() as u64);
    assert_eq!(
        fs::read(&file).unwrap(),
        before,
        "Verification must be read-only"
    );
    let mut tampered = before.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 1;
    fs::write(&file, &tampered).unwrap();
    assert!(verify_finalized(&output).unwrap_err().contains("CRC"));
    fs::write(&file, &before).unwrap();
    fs::remove_file(&file).unwrap();
    assert!(verify_finalized(&output).is_err());
    // Preserve a valid fixture for the independent offline decoder workflow.
    fs::write(&file, &before).unwrap();
    assert!(verification_allowed(&RecorderState::Idle, false));
    assert!(!verification_allowed(&RecorderState::Recording, false));
    assert!(!verification_allowed(&RecorderState::Paused, false));
    assert!(!verification_allowed(&RecorderState::Idle, true));
}
