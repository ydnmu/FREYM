//! Container lifecycle boundary.
//!
//! [`ContainerWriter`] is the contract. [`MkvVideoWriter`] writes a single H.264
//! Annex-B video track into Matroska with the caller's real variable frame
//! timestamps; [`MkvAvWriter`] adds the audio plan's Opus tracks to the same
//! container through the same EBML backend. [`recover`] restores the complete prefix
//! of an unfinished file into a new container; A/V drift correction is not
//! implemented.
use frame_contracts::{RecorderError, RecordingArtifact};

mod annexb;
mod av1;

/// Extract the sequence header from a native AV1 temporal unit for immutable
/// Tracks/av1C. Frame/tile data is never used as decoder configuration.
pub fn av1_sequence_configuration(
    data: &[u8],
) -> Result<(Vec<u8>, frame_encode::VideoColour), String> {
    let payload = av1::sequence_payload_from_packet(data)?;
    let mut config = vec![0x0a];
    let mut length = payload.len();
    loop {
        let byte = (length & 0x7f) as u8;
        length >>= 7;
        config.push(byte | if length != 0 { 0x80 } else { 0 });
        if length == 0 {
            break;
        }
    }
    config.extend_from_slice(payload);
    let (_, sequence) = av1::configuration(&config)?;
    Ok((
        config,
        frame_encode::VideoColour {
            bits_per_channel: sequence.depth,
            matrix_coefficients: sequence.matrix,
            transfer_characteristics: sequence.transfer,
            primaries: sequence.primaries,
            range: sequence.range,
            content_light: None,
            mastering: None,
        },
    ))
}

/// Codec-specific picture validation used by native producers before muxing.
pub fn validate_av1_access_unit(data: &[u8], config: Option<&[u8]>) -> Result<bool, String> {
    av1::validate_packet(data, config.map(av1::sequence_payload).transpose()?)
}
mod hevc;
mod journal;
mod mkv;
mod session;
pub use session::{
    RecordingSession, SegmentState, SessionManifest, SessionRecoverySummary, SessionSegment,
    read_session_manifest, recover_session, verify_session_segment,
};

pub use journal::{MAX_INTERRUPTED_RECORDINGS, interrupted_recordings};

pub use mkv::av::{AvAudioTrack, MkvAvWriter, MkvAvWriterStats};
pub use mkv::raster::MkvRasterWriter;
pub mod replay;
pub use mkv::recovery::{
    DeclaredTrack, MAX_RECOVERY_ELEMENT_DEPTH, MAX_RECOVERY_WORKING_BYTES, RecoveredTrack,
    RecoveredTrackKind, RecoverySummary, SkippedElement, TailKind, UnknownTail, declared_tracks,
    recover,
};
pub use mkv::{
    DurableCheckpoint, FIRST_AUDIO_TRACK_NUMBER, MAX_AUDIO_TRACKS, MAX_CODEC_CONFIG_BYTES,
    MAX_KEYFRAMES, MAX_PACKET_BYTES, MAX_RETAINED_BYTES, MkvVideoWriter, MkvVideoWriterStats,
    TIMESTAMP_SCALE_NS, VIDEO_TRACK_NUMBER,
};

pub trait ContainerWriter<Packet> {
    fn write(&mut self, packet: Packet) -> Result<(), RecorderError>;
    /// Success only after the writer completes durable finalization.
    fn finalize(self) -> Result<RecordingArtifact, RecorderError>;
}

/// Conservative restart admission: require an actual H.264 IDR first slice or
/// single-layer HEVC IDR/BLA first slice, independently of the packet keyframe flag.
pub fn independently_decodable_start(format: frame_encode::BitstreamFormat, data: &[u8]) -> bool {
    let mut first_slice = false;
    let mut dependent = false;
    let valid = match format {
        frame_encode::BitstreamFormat::H264AnnexB => annexb::for_each_nal(data, |kind, nal| {
            if (1..=5).contains(&kind) {
                dependent |= kind != 5;
                first_slice |= kind == 5 && nal.get(1).is_some_and(|byte| byte & 0x80 != 0);
            }
            Ok(())
        })
        .is_ok(),
        frame_encode::BitstreamFormat::HevcAnnexB => hevc::for_each_nal(data, |kind, nal| {
            if nal[0] & 1 != 0 || nal[1] >> 3 != 0 {
                return Err(hevc::HevcError::InvalidNal);
            }
            if kind <= 31 {
                dependent |= !(16..=20).contains(&kind);
                first_slice |=
                    (16..=20).contains(&kind) && nal.get(2).is_some_and(|byte| byte & 0x80 != 0);
            }
            Ok(())
        })
        .is_ok(),
        frame_encode::BitstreamFormat::Av1Obu => {
            return av1::validate_packet(data, None).unwrap_or(false);
        }
    };
    valid && first_slice && !dependent
}

#[cfg(test)]
mod restart_tests {
    use super::*;
    #[test]
    fn restart_gate_requires_coded_independence_and_refuses_cra_and_mixed_pictures() {
        use frame_encode::BitstreamFormat::*;
        assert!(independently_decodable_start(
            H264AnnexB,
            &[0, 0, 1, 0x65, 0x80]
        ));
        assert!(!independently_decodable_start(
            H264AnnexB,
            &[0, 0, 1, 0x41, 0x80]
        ));
        assert!(!independently_decodable_start(
            H264AnnexB,
            &[0, 0, 1, 0x65, 0x40]
        ));
        assert!(!independently_decodable_start(
            H264AnnexB,
            &[0, 0, 1, 0x65, 0x80, 0, 0, 1, 0x41, 0x80]
        ));
        assert!(independently_decodable_start(
            HevcAnnexB,
            include_bytes!("../tests/fixtures/synthetic-main10.hevc")
        ));
        assert!(!independently_decodable_start(
            HevcAnnexB,
            &[0, 0, 1, 21 << 1, 1, 0x80]
        ));
    }
}
