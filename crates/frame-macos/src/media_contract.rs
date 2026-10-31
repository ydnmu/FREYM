//! Validation shared by the native seam and device-free contract checks.
use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus};

pub(crate) const BGRA: u32 = u32::from_be_bytes(*b"BGRA");
pub(crate) const MAX_CODEC_CONFIG: usize = 128 * 1024;

fn failure(message: &str) -> RecorderError {
    RecorderError {
        code: ErrorCode::EncoderUnavailable,
        message: message.into(),
        recovery: RecoveryStatus::NoRecording,
        action: "Stop and start a new recording after the source or codec configuration changes."
            .into(),
    }
}

pub(crate) fn surface(
    expected: (u32, u32),
    observed: (u32, u32),
    pixel_format: u32,
) -> Result<(), RecorderError> {
    if expected.0 == 0 || expected.1 == 0 || expected != observed || pixel_format != BGRA {
        return Err(failure(
            "The captured surface does not match the fixed BGRA video configuration.",
        ));
    }
    Ok(())
}

pub(crate) fn timestamp(
    previous: Option<u64>,
    pts: u64,
    duration_ns: u64,
) -> Result<(), RecorderError> {
    // The bridge passes CMTime values as signed 64-bit nanoseconds.
    if pts > i64::MAX as u64
        || duration_ns == 0
        || duration_ns > i64::MAX as u64
        || pts
            .checked_add(duration_ns)
            .is_none_or(|end| end > i64::MAX as u64)
        || previous.is_some_and(|last| pts <= last)
    {
        return Err(failure(
            "The video timestamp is invalid or did not advance within this segment.",
        ));
    }
    Ok(())
}

pub(crate) fn packet(
    previous: Option<u64>,
    pts: u64,
    dts: u64,
    duration_ns: u64,
) -> Result<(), RecorderError> {
    timestamp(previous, pts, duration_ns)?;
    if dts != pts {
        return Err(failure(
            "VideoToolbox reordered output despite the fixed non-reordering track.",
        ));
    }
    Ok(())
}

pub(crate) fn codec_config(frozen: &[u8], observed: &[u8]) -> Result<(), RecorderError> {
    if observed.is_empty()
        || observed.len() > MAX_CODEC_CONFIG
        || (!frozen.is_empty() && frozen != observed)
    {
        return Err(failure(
            "VideoToolbox changed or returned an invalid H.264 track configuration.",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_surface_accepts_bgra_and_rejects_resize_or_hdr() {
        surface((1920, 1080), (1920, 1080), BGRA).unwrap();
        assert!(surface((1920, 1080), (1080, 1920), BGRA).is_err());
        assert!(surface((1920, 1080), (1920, 1080), u32::from_be_bytes(*b"x420")).is_err());
    }

    #[test]
    fn signed_cmtime_and_non_reordering_output_are_enforced() {
        packet(Some(10), 20, 20, 10).unwrap();
        for (pts, dts, duration) in [
            (10, 10, 10),
            (20, 19, 10),
            (20, 20, 0),
            (u64::MAX, u64::MAX, 1),
            (i64::MAX as u64, i64::MAX as u64, 1),
        ] {
            assert!(packet(Some(10), pts, dts, duration).is_err());
        }
    }

    #[test]
    fn codec_private_data_freezes_after_first_output() {
        codec_config(&[], &[0, 0, 0, 1, 0x67]).unwrap();
        codec_config(&[1, 2], &[1, 2]).unwrap();
        assert!(codec_config(&[1, 2], &[1, 3]).is_err());
        assert!(codec_config(&[], &[]).is_err());
        assert!(codec_config(&[], &vec![0; MAX_CODEC_CONFIG + 1]).is_err());
    }
}
