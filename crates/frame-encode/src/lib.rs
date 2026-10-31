//! Hardware encoder contracts, packet ownership and synchronous Opus track encoding.
pub mod admission;
pub mod audio_pipeline;
pub mod av1;
pub mod opus;

use frame_clock::MediaTimestamp;
use frame_contracts::RecorderError;
use std::num::{NonZeroU32, NonZeroU64};

#[derive(Debug, Clone)]
pub struct EncoderCapability {
    pub id: String,
    pub codec: String,
    pub hardware: bool,
}
pub trait EncoderProbe {
    fn probe(&self) -> Result<Vec<EncoderCapability>, RecorderError>;
}
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    pub track_id: u32,
    pub pts: MediaTimestamp,
    pub dts: MediaTimestamp,
    pub duration_ns: u64,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

/// Native media metadata, not an IPC model. No default provider or capability claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitstreamFormat {
    H264AnnexB,
    HevcAnnexB,
    /// AV1 low-overhead OBUs, each carrying its own bounded size field.
    Av1Obu,
}

/// Video signal properties carried from the source through encoding to the container.
/// Values map to the corresponding Matroska Colour and ITU-T H.273 code points.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VideoColour {
    pub bits_per_channel: u8,
    pub matrix_coefficients: u8,
    pub transfer_characteristics: u8,
    pub primaries: u8,
    /// Matroska Range: 1 is broadcast/limited, 2 is full.
    pub range: u8,
    /// Source-supplied only. Absence must never be replaced with nominal HDR values.
    pub content_light: Option<ContentLight>,
    pub mastering: Option<MasteringDisplay>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContentLight {
    pub max_cll: u16,
    pub max_fall: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MasteringDisplay {
    pub red: (f64, f64),
    pub green: (f64, f64),
    pub blue: (f64, f64),
    pub white: (f64, f64),
    pub luminance_max: f64,
    pub luminance_min: f64,
}

impl VideoColour {
    pub const BT709_LIMITED: Self = Self {
        bits_per_channel: 8,
        matrix_coefficients: 1,
        transfer_characteristics: 1,
        primaries: 1,
        range: 1,
        content_light: None,
        mastering: None,
    };

    /// Reject invalid signal/metadata before it reaches an encoder or container.
    pub fn validate(self) -> Result<(), RecorderError> {
        if !matches!(self.bits_per_channel, 8 | 10 | 16)
            || !matches!(self.range, 1 | 2)
            || !matches!(self.primaries, 1 | 2 | 9)
            || !matches!(self.matrix_coefficients, 0 | 1 | 2 | 9)
            || !matches!(self.transfer_characteristics, 1 | 2 | 8 | 13 | 16 | 18)
        {
            return Err(RecorderError::invalid("Unsupported video colour signal."));
        }
        if let Some(m) = self.mastering {
            for (x, y) in [m.red, m.green, m.blue, m.white] {
                if !x.is_finite()
                    || !y.is_finite()
                    || !(0.0..=1.0).contains(&x)
                    || !(0.0..=1.0).contains(&y)
                    || x + y > 1.00001
                {
                    return Err(RecorderError::invalid("Invalid mastering chromaticity."));
                }
            }
            if !m.luminance_min.is_finite()
                || !m.luminance_max.is_finite()
                || m.luminance_min < 0.0
                || m.luminance_max < m.luminance_min
                || m.luminance_max > 429496.7295
            {
                return Err(RecorderError::invalid("Invalid mastering luminance."));
            }
        }
        Ok(())
    }

    pub const BT2020_PQ_LIMITED: Self = Self {
        bits_per_channel: 10,
        matrix_coefficients: 9,
        transfer_characteristics: 16,
        primaries: 9,
        range: 1,
        content_light: None,
        mastering: None,
    };
}

#[derive(Debug, Clone)]
pub struct EncodedStreamInfo {
    pub capability: EncoderCapability,
    pub width: NonZeroU32,
    pub height: NonZeroU32,
    /// Configuration rate; never a measured input or output frame rate.
    pub nominal_rate_num: NonZeroU32,
    pub nominal_rate_den: NonZeroU32,
    pub bitstream_format: BitstreamFormat,
    /// Present only when the source and encoder actually preserve this signal.
    pub colour: Option<VideoColour>,
    /// Codec-defined initialization bytes; for Annex B, start-code-delimited SPS/PPS.
    pub codec_config: Vec<u8>,
    pub allows_reordering: bool,
}

/// Supplies duration without duplicating the input's surface/timestamp model.
/// The first native implementation uses Input = CapturedFrame<WindowsSurface>.
pub struct EncoderInput<Input> {
    pub input: Input,
    pub duration_ns: NonZeroU64,
}

/// Pressure returns ownership; it is not an error or permission to drop accepted work.
pub enum SubmitOutcome<Input> {
    Accepted,
    Backpressure(Input),
}

#[derive(Debug)]
pub enum EncoderPoll {
    Packet(EncodedPacket),
    Pending,
    /// EOS acknowledged, every accepted input accounted for and output queue empty.
    Drained,
}

/// Derive a VFR frame's duration from the next source timestamp. No rounding or
/// saturating arithmetic may conceal a repeated/backwards observation.
pub fn duration_until(
    pts: MediaTimestamp,
    next: MediaTimestamp,
) -> Result<NonZeroU64, RecorderError> {
    next.0
        .checked_sub(pts.0)
        .and_then(NonZeroU64::new)
        .ok_or_else(|| RecorderError::invalid("The next encoder timestamp must be strictly later."))
}

/// Deliberate final-frame duration policy: one nominal period, rounded up to ns.
pub fn nominal_duration(num: NonZeroU32, den: NonZeroU32) -> NonZeroU64 {
    let nanos = (1_000_000_000u64 * u64::from(den.get())).div_ceil(u64::from(num.get()));
    NonZeroU64::new(nanos).expect("positive rational period rounded up is nonzero")
}

pub trait PacketEncoder<Input> {
    /// Nonblocking admission. Backpressure preserves input; Err is terminal or an
    /// invalid request and does not accept this input. Accepted input is never silently lost.
    fn submit(&mut self, input: Input) -> Result<SubmitOutcome<Input>, RecorderError>;
    /// Nonblocking. Pending is not drain completion. Errors are sticky.
    fn try_packet(&mut self) -> Result<EncoderPoll, RecorderError>;
    fn stream_info(&self) -> &EncodedStreamInfo;
    /// Idempotently close admission and request EOS after previously accepted input.
    /// A full data queue cannot prevent this control operation.
    fn begin_drain(&mut self) -> Result<(), RecorderError>;
    /// Idempotent teardown/join on a control thread. Success requires completed drain
    /// and consumed output. Premature stop still cleans up but reports finalization failure.
    /// Drop performs cleanup too; it never detaches a live native worker.
    fn stop(&mut self) -> Result<(), RecorderError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variable_intervals_preserve_exact_source_time() {
        let times = [100, 101, 1_000_000_000, u64::MAX];
        for pair in times.windows(2) {
            let duration =
                duration_until(MediaTimestamp(pair[0]), MediaTimestamp(pair[1])).unwrap();
            assert_eq!(pair[0].checked_add(duration.get()), Some(pair[1]));
        }
    }

    #[test]
    fn duplicate_or_backwards_timestamps_are_errors() {
        for next in [0, 9, 10] {
            assert!(duration_until(MediaTimestamp(10), MediaTimestamp(next)).is_err());
        }
    }

    #[test]
    fn nominal_tail_duration_preserves_fractional_rates_without_zero() {
        assert_eq!(
            nominal_duration(
                NonZeroU32::new(60_000).unwrap(),
                NonZeroU32::new(1001).unwrap()
            )
            .get(),
            16_683_334
        );
        assert_eq!(
            nominal_duration(
                NonZeroU32::new(u32::MAX).unwrap(),
                NonZeroU32::new(1).unwrap()
            )
            .get(),
            1
        );
        assert_eq!(
            nominal_duration(
                NonZeroU32::new(1).unwrap(),
                NonZeroU32::new(u32::MAX).unwrap()
            )
            .get(),
            4_294_967_295_000_000_000
        );
    }
}
