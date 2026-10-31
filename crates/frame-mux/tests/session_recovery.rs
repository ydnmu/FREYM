use frame_clock::MediaTimestamp;
use frame_encode::{
    BitstreamFormat, EncodedPacket, EncodedStreamInfo, EncoderCapability, VideoColour,
};
use frame_mux::{
    ContainerWriter, RecordingSession, SegmentState, read_session_manifest, recover_session,
};
use std::{num::NonZeroU32, path::PathBuf};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let root = std::env::temp_dir().join("freym-session-tests");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
fn stream() -> EncodedStreamInfo {
    EncodedStreamInfo {
        capability: EncoderCapability {
            id: "fixture".into(),
            codec: "hevc".into(),
            hardware: false,
        },
        width: NonZeroU32::new(64).unwrap(),
        height: NonZeroU32::new(64).unwrap(),
        nominal_rate_num: NonZeroU32::new(1).unwrap(),
        nominal_rate_den: NonZeroU32::new(1).unwrap(),
        bitstream_format: BitstreamFormat::HevcAnnexB,
        colour: Some(VideoColour::BT2020_PQ_LIMITED),
        codec_config: include_bytes!("fixtures/synthetic-main10.hevc").to_vec(),
        allows_reordering: false,
    }
}
fn packet(pts: u64) -> EncodedPacket {
    EncodedPacket {
        track_id: 1,
        pts: MediaTimestamp(pts),
        dts: MediaTimestamp(pts),
        duration_ns: 1_000_000_000,
        keyframe: true,
        data: include_bytes!("fixtures/synthetic-main10.hevc").to_vec(),
    }
}

#[test]
fn metadata_manifest_roundtrip_and_recovery_preserve_media_and_event_prefix() {
    use frame_clock::{MasterClock, PauseTimeline};
    use frame_metadata::{
        Capabilities, EventKind, Header, Lifecycle, MetadataOptions, MetadataRecorder,
        SCHEMA_VERSION,
    };
    use std::sync::Arc;
    let dir = Scratch::new();
    let path = dir.0.join("metadata.mkv.freym-session");
    let mut session = RecordingSession::create(&path).unwrap();
    let mut writer = session
        .create_video_segment(&stream(), MediaTimestamp(0))
        .unwrap();
    let mut metadata = MetadataRecorder::start(
        &session.metadata_path(),
        Arc::new(MasterClock::default()),
        PauseTimeline::default(),
        Header {
            version: SCHEMA_VERSION,
            origin_ns: 0,
            cursor_baked: None,
            capabilities: Capabilities::default(),
        },
        MetadataOptions::default(),
    )
    .unwrap();
    session.attach_metadata().unwrap();
    assert_eq!(read_session_manifest(&path).unwrap().version, 2);
    metadata.lifecycle(Lifecycle::Started).unwrap();
    assert!(
        metadata
            .ingress()
            .try_emit(EventKind::Cursor { x: 20, y: 30 })
    );
    writer.write(packet(0)).unwrap();
    writer.finalize().unwrap();
    metadata.finish(false).unwrap();
    drop(metadata);
    let original = session.snapshot().unwrap();
    let media = std::fs::read(dir.0.join(&original.segments[0].file_name)).unwrap();
    let events_path = dir.0.join(original.metadata_file.as_ref().unwrap());
    let mut events = std::fs::read(&events_path).unwrap();
    events.extend_from_slice(b"damaged tail");
    std::fs::write(&events_path, &events).unwrap();
    drop(session);
    let output = dir.0.join("recovered-metadata.freym-session");
    let recovered = recover_session(&path, &output).unwrap();
    let summary = recovered.metadata_recovery.unwrap();
    assert_eq!(summary.events, 3);
    assert!(!summary.completed && !summary.clean_end);
    assert!(recovered.metadata_error.is_none());
    assert_eq!(recovered.manifest.version, 2);
    assert_eq!(
        std::fs::read(dir.0.join(&recovered.manifest.segments[0].file_name)).unwrap(),
        media
    );
    assert_eq!(std::fs::read(&events_path).unwrap(), events);
    let restored_events = dir
        .0
        .join(recovered.manifest.metadata_file.as_ref().unwrap());
    assert!(
        !frame_metadata::read_sidecar(&restored_events, |_| Ok(()))
            .unwrap()
            .completed
    );
    assert_eq!(read_session_manifest(&output).unwrap(), recovered.manifest);
}

#[test]
fn finalized_segments_are_copied_exactly_and_interrupted_tail_recovers_separately() {
    let dir = Scratch::new();
    // Preserve the requested first output path even without a .mkv extension.
    let path = dir.0.join("recording.capture.freym-session");
    let mut session = RecordingSession::create(&path).unwrap();
    let mut first = session
        .create_video_segment(&stream(), MediaTimestamp(7_000_000_000))
        .unwrap();
    first.write(packet(7_000_000_000)).unwrap();
    first.finalize().unwrap();
    let closed = session.snapshot().unwrap().segments[0].clone();
    assert_eq!(closed.file_name, "recording.capture");
    assert_eq!(closed.state, SegmentState::Closed);
    assert!(closed.cues > 0);
    let original = std::fs::read(dir.0.join(&closed.file_name)).unwrap();
    let mut tail = session
        .create_video_segment(&stream(), MediaTimestamp(9_000_000_000))
        .unwrap();
    tail.write(packet(9_000_000_000)).unwrap();
    let point = tail.checkpoint().unwrap();
    session
        .record_interruption_reason("fixture encoder failure")
        .unwrap();
    assert_eq!(
        session.snapshot().unwrap().segments[1].encoder_id,
        "fixture"
    );
    assert_eq!(
        session.snapshot().unwrap().segments[1]
            .interruption_reason
            .as_deref(),
        Some("fixture encoder failure")
    );
    assert_eq!(
        read_session_manifest(&path).unwrap().segments[1].checkpoint,
        Some(point)
    );
    drop(tail);
    drop(session);
    let output = dir.0.join("restored.freym-session");
    let recovered = recover_session(&path, &output).unwrap();
    assert_eq!(recovered.verified_closed_segments, 1);
    assert_eq!(recovered.recovered_tails.len(), 1);
    assert_eq!(recovered.manifest.segments[0].state, SegmentState::Closed);
    assert_eq!(
        std::fs::read(dir.0.join(&recovered.manifest.segments[0].file_name)).unwrap(),
        original
    );
    assert_eq!(
        std::fs::read(dir.0.join(&closed.file_name)).unwrap(),
        original
    );
    assert!(!recovered.manifest.completed);
    assert!(recover_session(&path, &output).is_err());
    // A structurally valid change to the interrupted file's width must not be
    // accepted under the manifest's original track configuration.
    let interrupted = read_session_manifest(&path).unwrap().segments[1].clone();
    let tail_path = dir.0.join(&interrupted.file_name);
    let mut changed = std::fs::read(&tail_path).unwrap();
    let header_at = changed
        .windows(interrupted.tracks_ebml.len())
        .position(|bytes| bytes == interrupted.tracks_ebml)
        .unwrap();
    let width_at = interrupted
        .tracks_ebml
        .windows(3)
        .position(|bytes| bytes == [0xb0, 0x81, 64])
        .unwrap();
    changed[header_at + width_at + 2] = 65;
    std::fs::write(&tail_path, &changed).unwrap();
    let mismatched = recover_session(&path, &dir.0.join("changed-tracks.session")).unwrap();
    assert_eq!(mismatched.verified_closed_segments, 1);
    assert!(mismatched.recovered_tails.is_empty());
    assert_eq!(mismatched.rejected_segments.len(), 1);
    assert!(mismatched.rejected_segments[0].1.contains("track header"));
    assert_eq!(std::fs::read(&tail_path).unwrap(), changed);
    assert_eq!(
        std::fs::read(dir.0.join(&closed.file_name)).unwrap(),
        original
    );
    println!("verified session fixture: {}", output.display());
}

#[test]
fn corrupt_manifest_or_closed_segment_never_becomes_verified_media() {
    let dir = Scratch::new();
    let path = dir.0.join("original.session");
    let mut session = RecordingSession::create(&path).unwrap();
    let mut writer = session
        .create_video_segment(&stream(), MediaTimestamp(0))
        .unwrap();
    writer.write(packet(0)).unwrap();
    writer.finalize().unwrap();
    let manifest = session.finish().unwrap();
    drop(session);
    let closed = dir.0.join(&manifest.segments[0].file_name);
    let mut media = std::fs::read(&closed).unwrap();
    media[0] ^= 1;
    std::fs::write(&closed, &media).unwrap();
    assert!(recover_session(&path, &dir.0.join("bad-media.session")).is_err());
    assert_eq!(std::fs::read(&closed).unwrap(), media);
    let mut index = std::fs::read(&path).unwrap();
    index[15] ^= 1;
    std::fs::write(&path, &index).unwrap();
    assert!(read_session_manifest(&path).is_err());
    assert!(recover_session(&path, &dir.0.join("bad-index.session")).is_err());
}

#[test]
fn failed_atomic_publication_retains_previous_checkpoint_and_prevents_finalize() {
    let dir = Scratch::new();
    let path = dir.0.join("publication.session");
    let mut session = RecordingSession::create(&path).unwrap();
    let mut writer = session
        .create_video_segment(&stream(), MediaTimestamp(0))
        .unwrap();
    writer.write(packet(0)).unwrap();
    // Finish the first async publication before injecting a collision into the
    // next generation; otherwise the fault injection races the existing job.
    writer.checkpoint().unwrap();
    let before = read_session_manifest(&path).unwrap();
    let temp = PathBuf::from(format!(
        "{}.tmp-{}-{}",
        path.display(),
        std::process::id(),
        before.generation + 1
    ));
    std::fs::write(temp, b"collision: publication must fail").unwrap();
    assert!(writer.checkpoint().is_err());
    assert_eq!(read_session_manifest(&path).unwrap(), before);
    assert!(writer.finalize().is_err());
    assert!(session.snapshot().is_err());
    assert!(
        session
            .create_video_segment(&stream(), MediaTimestamp(2_000_000_000))
            .is_err()
    );
}

#[test]
fn recorded_interruption_cannot_finalize_and_new_origin_cannot_regress() {
    let dir = Scratch::new();
    let path = dir.0.join("fault.session");
    let mut session = RecordingSession::create(&path).unwrap();
    let mut old = session
        .create_video_segment(&stream(), MediaTimestamp(7_000_000_000))
        .unwrap();
    old.write(packet(7_000_000_000)).unwrap();
    session
        .record_interruption_reason("published encoder fault before preparation")
        .unwrap();
    assert!(
        old.finalize()
            .unwrap_err()
            .message
            .contains("interrupted segment")
    );
    let interrupted = session.snapshot().unwrap();
    assert_eq!(interrupted.segments[0].state, SegmentState::Interrupted);
    assert_eq!(interrupted.segments[0].file_bytes, 0);
    assert!(!session.finish().unwrap().completed);
    let before = read_session_manifest(&path).unwrap();
    assert!(
        session
            .create_video_segment(&stream(), MediaTimestamp(6_000_000_000))
            .is_err()
    );
    assert_eq!(read_session_manifest(&path).unwrap(), before);
    let mut next = session
        .create_video_segment(&stream(), MediaTimestamp(9_000_000_000))
        .unwrap();
    next.write(packet(9_000_000_000)).unwrap();
    next.finalize().unwrap();
    assert!(!session.finish().unwrap().completed);
}

#[test]
fn an_earlier_corrupt_segment_does_not_hide_a_later_verified_segment() {
    let dir = Scratch::new();
    let path = dir.0.join("restart.session");
    let mut session = RecordingSession::create(&path).unwrap();
    let mut old = session
        .create_video_segment(&stream(), MediaTimestamp(0))
        .unwrap();
    old.write(packet(0)).unwrap();
    old.checkpoint().unwrap();
    session
        .record_interruption_reason("injected worker failure")
        .unwrap();
    drop(old);
    let mut next = session
        .create_video_segment(&stream(), MediaTimestamp(2_000_000_000))
        .unwrap();
    next.write(packet(2_000_000_000)).unwrap();
    next.finalize().unwrap();
    let manifest = session.finish().unwrap();
    assert!(!manifest.completed);
    drop(session);
    let old_path = dir.0.join(&manifest.segments[0].file_name);
    let corrupt = b"the interrupted segment has no playable prefix";
    std::fs::write(&old_path, corrupt).unwrap();
    let next_path = dir.0.join(&manifest.segments[1].file_name);
    let verified = std::fs::read(&next_path).unwrap();
    let recovered = recover_session(&path, &dir.0.join("safe-recovery.session")).unwrap();
    assert_eq!(recovered.verified_closed_segments, 1);
    assert_eq!(recovered.rejected_segments.len(), 1);
    assert!(recovered.recovered_tails.is_empty());
    assert_eq!(recovered.manifest.segments[0].origin_ns, 2_000_000_000);
    assert_eq!(recovered.manifest.segments[0].state, SegmentState::Closed);
    assert!(!recovered.manifest.completed);
    assert_eq!(
        std::fs::read(dir.0.join(&recovered.manifest.segments[0].file_name)).unwrap(),
        verified
    );
    assert_eq!(std::fs::read(&next_path).unwrap(), verified);
    assert_eq!(std::fs::read(&old_path).unwrap(), corrupt);
    println!(
        "later verified segment preservation fixture: {}",
        dir.0.display()
    );
}

#[test]
#[ignore = "child process fixture"]
fn session_process_fixture() {
    let path =
        PathBuf::from(std::env::var_os("FREYM_SESSION_FIXTURE").expect("parent supplies fixture"));
    let mut session = RecordingSession::create(&path).unwrap();
    let mut first = session
        .create_video_segment(&stream(), MediaTimestamp(0))
        .unwrap();
    first.write(packet(0)).unwrap();
    first.finalize().unwrap();
    let mut tail = session
        .create_video_segment(&stream(), MediaTimestamp(2_000_000_000))
        .unwrap();
    tail.write(packet(2_000_000_000)).unwrap();
    tail.checkpoint().unwrap();
    std::fs::write(path.with_extension("ready"), b"checkpoint published").unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn process_crash_retains_verified_closed_segment_and_synced_open_prefix() {
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let dir = Scratch::new();
    let path = dir.0.join("crash.freym-session");
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--ignored",
            "--exact",
            "session_process_fixture",
            "--nocapture",
        ])
        .env("FREYM_SESSION_FIXTURE", &path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = Child(command.spawn().unwrap());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !path.with_extension("ready").exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "Fixture exited before its durable checkpoint"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "Fixture preparation expired"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let before = read_session_manifest(&path).unwrap();
    assert_eq!(before.segments[0].state, SegmentState::Closed);
    assert_eq!(before.segments[1].state, SegmentState::Open);
    let closed = std::fs::read(dir.0.join(&before.segments[0].file_name)).unwrap();
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    assert_eq!(read_session_manifest(&path).unwrap(), before);
    assert!(
        frame_mux::interrupted_recordings(&dir.0)
            .unwrap()
            .0
            .contains(&path)
    );
    let recovery = recover_session(&path, &dir.0.join("crash-restored.freym-session")).unwrap();
    assert_eq!(recovery.verified_closed_segments, 1);
    assert_eq!(recovery.recovered_tails.len(), 1);
    assert_eq!(
        std::fs::read(dir.0.join(&recovery.manifest.segments[0].file_name)).unwrap(),
        closed
    );
    assert_eq!(
        std::fs::read(dir.0.join(&before.segments[0].file_name)).unwrap(),
        closed
    );
    println!("process crash proof: {}", dir.0.display());
}
