use frame_clock::MediaTimestamp;
use frame_encode::{
    BitstreamFormat, EncodedPacket, EncodedStreamInfo, EncoderCapability, VideoColour,
};
use frame_mux::{ContainerWriter, MkvVideoWriter, recover};
use std::{
    num::NonZeroU32,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

const FIXTURE: &[u8] = include_bytes!("fixtures/synthetic-main10.av1");
fn directory() -> PathBuf {
    let root = std::env::var_os("FREYM_AV1_EVIDENCE")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("freym-av1-tests"));
    let path = root.join(format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}
fn stream() -> EncodedStreamInfo {
    // The fixture consists of a size-bearing temporal delimiter, sequence and Frame OBU.
    let config_end = 2 + 2 + usize::from(FIXTURE[3]);
    EncodedStreamInfo {
        capability: EncoderCapability {
            id: "synthetic-av1-main10".into(),
            codec: "av1".into(),
            hardware: false,
        },
        width: NonZeroU32::new(64).unwrap(),
        height: NonZeroU32::new(64).unwrap(),
        nominal_rate_num: NonZeroU32::new(1).unwrap(),
        nominal_rate_den: NonZeroU32::new(1).unwrap(),
        bitstream_format: BitstreamFormat::Av1Obu,
        colour: Some(VideoColour::BT2020_PQ_LIMITED),
        codec_config: FIXTURE[..config_end].to_vec(),
        allows_reordering: false,
    }
}
fn packet(bytes: Vec<u8>, keyframe: bool) -> EncodedPacket {
    EncodedPacket {
        track_id: 1,
        pts: MediaTimestamp(0),
        dts: MediaTimestamp(0),
        duration_ns: 1_000_000_000,
        keyframe,
        data: bytes,
    }
}

#[test]
fn main10_av1_mux_and_recovery_keep_colour_configuration_and_original() {
    let directory = directory();
    let path = directory.join("main10.mkv");
    let copy = directory.join("recovered.mkv");
    assert!(frame_mux::independently_decodable_start(
        BitstreamFormat::Av1Obu,
        FIXTURE
    ));
    let mut writer = MkvVideoWriter::create(&path, &stream(), MediaTimestamp(0)).unwrap();
    writer.write(packet(FIXTURE.to_vec(), true)).unwrap();
    writer.finalize().unwrap();
    let original = std::fs::read(&path).unwrap();
    let summary = recover(&path, &copy).unwrap();
    assert!(summary.source_finalized);
    assert_eq!(summary.recovered_video_packets, 1);
    assert_eq!(summary.recovered_keyframes, 1);
    assert_eq!(summary.tracks[0].codec_id, "V_AV1");
    assert_eq!(std::fs::read(&path).unwrap(), original);
    for bytes in [std::fs::read(&path).unwrap(), std::fs::read(&copy).unwrap()] {
        for id in [
            b"V_AV1".as_slice(),
            b"\x55\xb0",
            b"\x55\xba",
            b"\x55\xbb",
            b"\x55\xb1",
        ] {
            assert!(bytes.windows(id.len()).any(|window| window == id));
        }
    }
    println!("AV1_DECODE_FIXTURE={}", directory.display());
}

#[test]
fn av1_rejects_lied_keys_changed_sequence_and_wrong_colour_before_payload_write() {
    let directory = directory();
    let path = directory.join("false-key.mkv");
    let mut writer = MkvVideoWriter::create(&path, &stream(), MediaTimestamp(0)).unwrap();
    assert!(writer.write(packet(FIXTURE.to_vec(), false)).is_err());
    assert_eq!(writer.stats().packets, 0);
    let mut wrong = stream();
    wrong.colour.as_mut().unwrap().transfer_characteristics = 1;
    let path = directory.join("wrong-colour.mkv");
    assert!(MkvVideoWriter::create(&path, &wrong, MediaTimestamp(0)).is_err());
    assert!(!path.exists());
    let path = directory.join("changed-sequence.mkv");
    let mut writer = MkvVideoWriter::create(&path, &stream(), MediaTimestamp(0)).unwrap();
    let mut changed = FIXTURE.to_vec();
    changed[4] ^= 0x08;
    assert!(writer.write(packet(changed, true)).is_err());
    assert_eq!(writer.stats().packets, 0);
    let path = directory.join("bounded.mkv");
    let mut writer = MkvVideoWriter::create(&path, &stream(), MediaTimestamp(0)).unwrap();
    assert!(
        writer
            .write(packet(
                vec![0x32, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
                true
            ))
            .is_err()
    );
    assert_eq!(writer.stats().packets, 0);
}

#[test]
fn av1_and_opus_recovery_preserve_track_trim() {
    use frame_audio::{TrackKind, TrackPlan};
    use frame_mux::{AvAudioTrack, MkvAvWriter};
    let directory = directory();
    let path = directory.join("av.mkv");
    let copy = directory.join("av-recovered.mkv");
    let audio = AvAudioTrack::from_plan(
        &TrackPlan {
            kind: TrackKind::MainMix,
            sources: vec![],
        },
        312,
    )
    .unwrap();
    let mut writer = MkvAvWriter::create(&path, &stream(), &[audio], MediaTimestamp(0)).unwrap();
    writer.write_video(packet(FIXTURE.to_vec(), true)).unwrap();
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
    let original = std::fs::read(&path).unwrap();
    let summary = recover(&path, &copy).unwrap();
    assert_eq!(summary.recovered_video_packets, 1);
    assert_eq!(summary.recovered_audio_packets, 1);
    assert_eq!(summary.tracks[0].codec_id, "V_AV1");
    assert_eq!(summary.tracks[1].codec_id, "A_OPUS");
    assert_eq!(summary.tracks[1].opus_pre_skip_samples, Some(312));
    assert_eq!(summary.tracks[1].last_discard_padding_ns, 10_000_000);
    assert!(summary.final_trim_known);
    assert_eq!(std::fs::read(&path).unwrap(), original);
    println!("AV1_AV_DECODE_FIXTURE={}", directory.display());
}
