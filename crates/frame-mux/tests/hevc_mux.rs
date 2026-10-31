use std::num::NonZeroU32;

use frame_clock::MediaTimestamp;
use frame_encode::{
    BitstreamFormat, EncodedPacket, EncodedStreamInfo, EncoderCapability, VideoColour,
};
use frame_mux::{ContainerWriter, MkvVideoWriter, TailKind, recover};

fn stream() -> EncodedStreamInfo {
    EncodedStreamInfo {
        capability: EncoderCapability {
            id: "synthetic".into(),
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

#[test]
fn main10_colour_and_recovery_preserve_the_original() {
    let dir = std::env::temp_dir().join(format!("frame-hevc-mux-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let output = dir.join("main10.mkv");
    let recovered = dir.join("recovered.mkv");
    let _ = std::fs::remove_file(&output);
    let _ = std::fs::remove_file(&recovered);
    let bytes = include_bytes!("fixtures/synthetic-main10.hevc").to_vec();
    let mut writer = MkvVideoWriter::create(&output, &stream(), MediaTimestamp(0)).unwrap();
    writer
        .write(EncodedPacket {
            track_id: 1,
            pts: MediaTimestamp(0),
            dts: MediaTimestamp(0),
            duration_ns: 1_000_000_000,
            keyframe: true,
            data: bytes,
        })
        .unwrap();
    writer.finalize().unwrap();
    let file = std::fs::read(&output).unwrap();
    assert!(
        file.windows(b"V_MPEGH/ISO/HEVC".len())
            .any(|window| window == b"V_MPEGH/ISO/HEVC")
    );
    // Matroska Colour and its primaries, transfer and matrix element IDs.
    for id in [
        b"\x55\xB0".as_slice(),
        b"\x55\xBB",
        b"\x55\xBA",
        b"\x55\xB1",
    ] {
        assert!(file.windows(id.len()).any(|window| window == id));
    }
    let summary = recover(&output, &recovered).unwrap();
    assert_eq!(summary.recovered_video_packets, 1);
    assert_eq!(summary.recovered_keyframes, 1);
    assert!(summary.source_finalized);
    assert_eq!(summary.tracks[0].codec_id, "V_MPEGH/ISO/HEVC");
    assert_eq!(
        std::fs::read(&output).unwrap(),
        file,
        "source remains unchanged"
    );
    let remuxed = std::fs::read(&recovered).unwrap();
    for id in [
        b"\x55\xB0".as_slice(),
        b"\x55\xBB",
        b"\x55\xBA",
        b"\x55\xB1",
    ] {
        assert!(remuxed.windows(id.len()).any(|window| window == id));
    }
    assert!(
        recover(&output, &recovered).is_err(),
        "existing recovery output stays protected"
    );
    std::fs::remove_file(output).unwrap();
    std::fs::remove_file(recovered).unwrap();
}

fn packet(pts: u64, keyframe: bool, data: Vec<u8>) -> EncodedPacket {
    EncodedPacket {
        track_id: 1,
        pts: MediaTimestamp(pts),
        dts: MediaTimestamp(pts),
        duration_ns: 1_000_000_000,
        keyframe,
        data,
    }
}

#[test]
fn a_cut_hevc_tail_recovers_the_closed_checkpoint_prefix() {
    let dir = std::env::temp_dir().join(format!("frame-hevc-cut-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let source = dir.join("cut.mkv");
    let output = dir.join("copy.mkv");
    let mut writer = MkvVideoWriter::create(&source, &stream(), MediaTimestamp(0)).unwrap();
    writer
        .write(packet(
            0,
            true,
            include_bytes!("fixtures/synthetic-main10.hevc").to_vec(),
        ))
        .unwrap();
    // Periodic file durability is asynchronous; cut only a confirmed prefix.
    let checkpoint = writer.checkpoint().unwrap();
    assert_eq!(writer.stats().last_checkpoint, Some(checkpoint));
    let mut dependent = vec![0, 0, 0, 1, 2, 1, 0x80];
    dependent.extend(vec![0x55; 32 * 1024]);
    writer
        .write(packet(1_000_000_000, false, dependent))
        .unwrap();
    drop(writer);
    let mut bytes = std::fs::read(&source).unwrap();
    assert!(bytes.len() as u64 > checkpoint.position_bytes + 20);
    bytes.truncate(bytes.len() - 20);
    std::fs::write(&source, &bytes).unwrap();
    let summary = recover(&source, &output).unwrap();
    assert_eq!(summary.recovered_video_packets, 1);
    assert_eq!(summary.recovered_keyframes, 1);
    assert_eq!(
        summary.unknown_tail.kind,
        TailKind::Truncated,
        "{:?}",
        summary.unknown_tail
    );
    assert!(!summary.source_finalized);
    assert!(!summary.final_trim_known);
    assert_eq!(summary.duration_ns, checkpoint.container_end_ns);
    assert_eq!(std::fs::read(&source).unwrap(), bytes);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn hevc_refuses_a_false_keyframe_without_creating_a_recovery_copy() {
    let dir =
        std::env::temp_dir().join(format!("frame-hevc-false-keyframe-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let source = dir.join("false.mkv");
    let output = dir.join("copy.mkv");
    let mut writer = MkvVideoWriter::create(&source, &stream(), MediaTimestamp(0)).unwrap();
    let error = writer
        .write(packet(0, true, vec![0, 0, 0, 1, 2, 1, 0x80, 0x55]))
        .unwrap_err();
    assert!(error.message.contains("keyframe"), "{error:?}");
    drop(writer);
    let before = std::fs::read(&source).unwrap();
    let error = recover(&source, &output).unwrap_err();
    assert!(error.message.contains("cluster"), "{error:?}");
    assert!(!output.exists());
    assert_eq!(std::fs::read(&source).unwrap(), before);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn hevc_and_opus_track_metadata_and_trim_survive_recovery() {
    use frame_audio::{TrackKind, TrackPlan};
    use frame_mux::{AvAudioTrack, MkvAvWriter};

    let dir = std::env::temp_dir().join(format!("frame-hevc-av-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let source = dir.join("av.mkv");
    let output = dir.join("copy.mkv");
    let audio = AvAudioTrack::from_plan(
        &TrackPlan {
            kind: TrackKind::MainMix,
            sources: vec![],
        },
        312,
    )
    .unwrap();
    let mut writer = MkvAvWriter::create(&source, &stream(), &[audio], MediaTimestamp(0)).unwrap();
    writer
        .write_video(packet(
            0,
            true,
            include_bytes!("fixtures/synthetic-main10.hevc").to_vec(),
        ))
        .unwrap();
    writer
        .write_audio(EncodedPacket {
            track_id: 0,
            pts: MediaTimestamp(0),
            dts: MediaTimestamp(0),
            duration_ns: 20_000_000,
            keyframe: true,
            data: vec![0xf8, 0xff, 0xfe],
        })
        .unwrap();
    writer.finish_audio(0, 480).unwrap();
    writer.finalize().unwrap();
    let bytes = std::fs::read(&source).unwrap();
    let summary = recover(&source, &output).unwrap();
    assert_eq!(summary.recovered_video_packets, 1);
    assert_eq!(summary.recovered_audio_packets, 1);
    assert_eq!(summary.tracks.len(), 2);
    assert_eq!(summary.tracks[0].codec_id, "V_MPEGH/ISO/HEVC");
    assert_eq!(summary.tracks[1].codec_id, "A_OPUS");
    assert_eq!(summary.tracks[1].opus_pre_skip_samples, Some(312));
    assert_eq!(summary.tracks[1].last_discard_padding_ns, 10_000_000);
    assert!(summary.final_trim_known);
    assert_eq!(std::fs::read(&source).unwrap(), bytes);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn main10_rejects_missing_source_colour() {
    let mut metadata = stream();
    metadata.colour = None;
    let path =
        std::env::temp_dir().join(format!("frame-hevc-no-colour-{}.mkv", std::process::id()));
    let _ = std::fs::remove_file(&path);
    assert!(MkvVideoWriter::create(&path, &metadata, MediaTimestamp(0)).is_err());
    assert!(!path.exists());
}
