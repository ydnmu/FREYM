//! Annex-B H.264 access units: parameter-set extraction and Matroska AVC framing.
//!
//! Matroska stores H.264 as length-prefixed NAL units and carries the parameter sets
//! in the track's CodecPrivate as an AVCDecoderConfigurationRecord. The encoder hands
//! this crate start-code-delimited Annex-B, so the conversion here only repackages
//! NAL payload bytes: every byte of the access unit is copied, and no picture is
//! decoded, re-encoded or resized.

use std::fmt;

/// Length prefix width used for every NAL unit. It must match the
/// `lengthSizeMinusOne` field written into the configuration record.
pub(crate) const NAL_LENGTH_BYTES: usize = 4;

const NAL_TYPE_SPS: u8 = 7;
const NAL_TYPE_PPS: u8 = 8;

/// Largest number of parameter sets an AVC configuration record can hold.
const MAX_SPS_ENTRIES: usize = 31;
const MAX_PPS_ENTRIES: usize = 255;

/// Why an Annex-B buffer could not be interpreted. The caller attaches the error
/// code and recovery status, because the same defect means different things while
/// preparing a file and while appending a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnnexBError {
    Empty,
    /// Bytes before the first start code, so the buffer is not a NAL byte stream.
    NotStartCodeDelimited {
        offset: usize,
    },
    /// Two start codes with no payload between them.
    EmptyNalUnit {
        offset: usize,
    },
    /// H.264 reserves NAL unit type 0 for unspecified non-conforming data.
    UnspecifiedNalUnitType {
        offset: usize,
    },
    /// A single unit that cannot carry a 32-bit length prefix.
    NalUnitTooLarge {
        bytes: usize,
    },
    /// A parameter set that cannot carry a 16-bit length prefix.
    ParameterSetTooLarge {
        bytes: usize,
    },
    MissingSps,
    MissingPps,
    TooManyParameterSets {
        sps: usize,
        pps: usize,
    },
    /// A sequence parameter set too short to contain profile/level bytes.
    ShortSps {
        bytes: usize,
    },
}

impl fmt::Display for AnnexBError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AnnexBError::Empty => write!(f, "the Annex-B buffer is empty"),
            AnnexBError::NotStartCodeDelimited { offset } => write!(
                f,
                "byte {offset} is not inside a start-code-delimited NAL unit, so the buffer is not Annex-B H.264"
            ),
            AnnexBError::EmptyNalUnit { offset } => {
                write!(f, "the NAL unit at byte {offset} has no payload")
            }
            AnnexBError::UnspecifiedNalUnitType { offset } => write!(
                f,
                "the NAL unit at byte {offset} uses type 0 or sets forbidden_zero_bit"
            ),
            AnnexBError::NalUnitTooLarge { bytes } => {
                write!(
                    f,
                    "a NAL unit of {bytes} bytes cannot carry a length prefix"
                )
            }
            AnnexBError::ParameterSetTooLarge { bytes } => write!(
                f,
                "a parameter set of {bytes} bytes does not fit the configuration record"
            ),
            AnnexBError::MissingSps => {
                write!(f, "the codec configuration has no sequence parameter set")
            }
            AnnexBError::MissingPps => {
                write!(f, "the codec configuration has no picture parameter set")
            }
            AnnexBError::TooManyParameterSets { sps, pps } => write!(
                f,
                "the codec configuration has {sps} sequence and {pps} picture parameter sets, more than the configuration record can describe"
            ),
            AnnexBError::ShortSps { bytes } => write!(
                f,
                "a sequence parameter set of {bytes} bytes is too short for profile and level"
            ),
        }
    }
}

pub(crate) type AnnexBResult<T> = Result<T, AnnexBError>;

/// Visits every NAL unit in one Annex-B access unit, in stream order.
///
/// The buffer must be start-code delimited from its first byte. Trailing zero bytes
/// that belong to the next start code (or pad the end of the stream) are excluded
/// from the payload, as H.264 defines them outside the NAL unit.
pub(crate) fn for_each_nal<'a>(
    data: &'a [u8],
    mut visit: impl FnMut(u8, &'a [u8]) -> AnnexBResult<()>,
) -> AnnexBResult<()> {
    if data.is_empty() {
        return Err(AnnexBError::Empty);
    }
    let mut cursor =
        start_code_at(data, 0).ok_or(AnnexBError::NotStartCodeDelimited { offset: 0 })?;
    let mut unit_start = cursor;
    loop {
        let next = find_start_code(data, cursor);
        let (payload_end, next_cursor) = match next {
            Some((prefix, after)) => {
                let mut end = prefix;
                while end > unit_start && data[end - 1] == 0 {
                    end -= 1;
                }
                (end, after)
            }
            None => {
                let mut end = data.len();
                while end > unit_start && data[end - 1] == 0 {
                    end -= 1;
                }
                (end, data.len())
            }
        };
        if payload_end == unit_start || (next.is_some() && next_cursor == data.len()) {
            return Err(AnnexBError::EmptyNalUnit { offset: unit_start });
        }
        let payload = &data[unit_start..payload_end];
        let nal_type = payload[0] & 0x1F;
        if nal_type == 0 || payload[0] & 0x80 != 0 {
            return Err(AnnexBError::UnspecifiedNalUnitType { offset: unit_start });
        }
        visit(nal_type, payload)?;
        if next_cursor >= data.len() {
            break;
        }
        cursor = next_cursor;
        unit_start = cursor;
    }
    Ok(())
}

/// Exact converted length without allocating.
pub(crate) fn access_unit_size(data: &[u8]) -> AnnexBResult<usize> {
    let mut size = 0usize;
    for_each_nal(data, |_, payload| {
        size = size
            .checked_add(4 + payload.len())
            .ok_or(AnnexBError::NalUnitTooLarge {
                bytes: payload.len(),
            })?;
        Ok(())
    })?;
    Ok(size)
}

/// Writes the access unit as length-prefixed NAL units into `out`, which is cleared
/// first. The payload bytes are copied unchanged apart from losing start codes.
pub(crate) fn length_prefixed_access_unit(data: &[u8], out: &mut Vec<u8>) -> AnnexBResult<()> {
    let size = access_unit_size(data)?;
    out.clear();
    out.reserve_exact(size);
    for_each_nal(data, |_, payload| {
        let length = u32::try_from(payload.len()).map_err(|_| AnnexBError::NalUnitTooLarge {
            bytes: payload.len(),
        })?;
        out.extend_from_slice(&length.to_be_bytes());
        out.extend_from_slice(payload);
        Ok(())
    })
}

/// Builds the AVCDecoderConfigurationRecord stored as Matroska CodecPrivate from the
/// encoder's start-code-delimited parameter sets.
pub(crate) fn avc_decoder_configuration_record(codec_config: &[u8]) -> AnnexBResult<Vec<u8>> {
    let mut sps: Vec<&[u8]> = Vec::new();
    let mut pps: Vec<&[u8]> = Vec::new();
    for_each_nal(codec_config, |nal_type, payload| {
        match nal_type {
            NAL_TYPE_SPS => sps.push(payload),
            NAL_TYPE_PPS => pps.push(payload),
            _ => {}
        }
        Ok(())
    })?;

    if sps.is_empty() {
        return Err(AnnexBError::MissingSps);
    }
    if pps.is_empty() {
        return Err(AnnexBError::MissingPps);
    }
    if sps.len() > MAX_SPS_ENTRIES || pps.len() > MAX_PPS_ENTRIES {
        return Err(AnnexBError::TooManyParameterSets {
            sps: sps.len(),
            pps: pps.len(),
        });
    }
    let profile = *sps[0].get(1).ok_or(AnnexBError::ShortSps {
        bytes: sps[0].len(),
    })?;
    let compatibility = *sps[0].get(2).ok_or(AnnexBError::ShortSps {
        bytes: sps[0].len(),
    })?;
    let level = *sps[0].get(3).ok_or(AnnexBError::ShortSps {
        bytes: sps[0].len(),
    })?;

    let mut record = Vec::with_capacity(11 + sps.len() * 2 + pps.len() * 2);
    record.push(1);
    record.push(profile);
    record.push(compatibility);
    record.push(level);
    let length_size_minus_one = u8::try_from(NAL_LENGTH_BYTES - 1).expect("four byte prefixes");
    record.push(0xFC | length_size_minus_one);
    record.push(0xE0 | u8::try_from(sps.len()).expect("at most 31 entries"));
    for set in &sps {
        push_parameter_set(&mut record, set)?;
    }
    record.push(u8::try_from(pps.len()).expect("at most 255 entries"));
    for set in &pps {
        push_parameter_set(&mut record, set)?;
    }
    Ok(record)
}

fn push_parameter_set(record: &mut Vec<u8>, set: &[u8]) -> AnnexBResult<()> {
    let length = u16::try_from(set.len())
        .map_err(|_| AnnexBError::ParameterSetTooLarge { bytes: set.len() })?;
    record.extend_from_slice(&length.to_be_bytes());
    record.extend_from_slice(set);
    Ok(())
}

/// Returns the offset just past a start code beginning at `offset`, if one does.
fn start_code_at(data: &[u8], offset: usize) -> Option<usize> {
    if data.len() >= offset + 4 && data[offset..offset + 4] == [0, 0, 0, 1] {
        return Some(offset + 4);
    }
    if data.len() >= offset + 3 && data[offset..offset + 3] == [0, 0, 1] {
        return Some(offset + 3);
    }
    None
}

/// Finds the next start code at or after `from`, returning the offset of its first
/// zero byte and the offset just past it.
fn find_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut index = from;
    while index + 3 <= data.len() {
        if data[index..index + 3] == [0, 0, 1] {
            if index > from && data[index - 1] == 0 {
                // A four byte start code; its first zero belongs to the prefix.
                return Some((index - 1, index + 3));
            }
            return Some((index, index + 3));
        }
        index += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: [u8; 8] = [0x67, 0x64, 0x00, 0x3C, 0xAC, 0x2B, 0x40, 0x14];
    const PPS: [u8; 4] = [0x68, 0xEE, 0x3C, 0xB0];

    fn config() -> Vec<u8> {
        let mut config = Vec::new();
        config.extend_from_slice(&[0, 0, 0, 1]);
        config.extend_from_slice(&SPS);
        config.extend_from_slice(&[0, 0, 1]);
        config.extend_from_slice(&PPS);
        config
    }

    #[test]
    fn configuration_record_describes_the_encoder_parameter_sets() {
        let record = avc_decoder_configuration_record(&config()).unwrap();
        assert_eq!(record[0], 1, "configuration version");
        assert_eq!(
            &record[1..4],
            &SPS[1..4],
            "profile, compatibility and level"
        );
        assert_eq!(record[4], 0xFF, "reserved bits and four byte lengths");
        assert_eq!(record[5], 0xE1, "one sequence parameter set");
        assert_eq!(&record[6..8], &[0, SPS.len() as u8]);
        assert_eq!(&record[8..8 + SPS.len()], &SPS);
        assert_eq!(record[8 + SPS.len()], 1, "one picture parameter set");
        assert_eq!(
            &record[9 + SPS.len()..11 + SPS.len()],
            &[0, PPS.len() as u8]
        );
        assert_eq!(&record[11 + SPS.len()..], &PPS);
    }

    #[test]
    fn access_unit_conversion_preserves_every_payload_byte() {
        let mut unit = Vec::new();
        unit.extend_from_slice(&[0, 0, 0, 1]);
        unit.extend_from_slice(&SPS);
        unit.extend_from_slice(&[0, 0, 0, 1]);
        unit.extend_from_slice(&PPS);
        unit.extend_from_slice(&[0, 0, 1]);
        unit.push(0x65);
        unit.extend_from_slice(&[0xAA, 0xBB, 0x00, 0x00]);

        let mut out = Vec::new();
        length_prefixed_access_unit(&unit, &mut out).unwrap();
        let mut expected = Vec::new();
        for nal in [&SPS[..], &PPS[..], &[0x65, 0xAA, 0xBB][..]] {
            expected.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            expected.extend_from_slice(nal);
        }
        assert_eq!(out, expected);
    }

    #[test]
    fn trailing_zero_padding_is_not_part_of_the_last_unit() {
        let mut unit = Vec::new();
        unit.extend_from_slice(&[0, 0, 0, 1, 0x41, 0x11, 0x22, 0x00, 0x00, 0x00]);
        let mut count = 0;
        for_each_nal(&unit, |nal_type, payload| {
            count += 1;
            assert_eq!(nal_type, 1);
            assert_eq!(payload, [0x41, 0x11, 0x22]);
            Ok(())
        })
        .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn malformed_buffers_are_rejected() {
        assert_eq!(for_each_nal(&[], |_, _| Ok(())), Err(AnnexBError::Empty));
        assert!(matches!(
            for_each_nal(&[0x41, 0x11], |_, _| Ok(())),
            Err(AnnexBError::NotStartCodeDelimited { .. })
        ));
        assert!(matches!(
            for_each_nal(&[0, 0, 0, 1, 0, 0, 1, 0x41], |_, _| Ok(())),
            Err(AnnexBError::EmptyNalUnit { .. })
        ));
        assert!(matches!(
            for_each_nal(&[0, 0, 1, 0x00, 0x11], |_, _| Ok(())),
            Err(AnnexBError::UnspecifiedNalUnitType { .. })
        ));
        assert!(matches!(
            avc_decoder_configuration_record(&[0, 0, 1, 0x68, 0xEE]),
            Err(AnnexBError::MissingSps)
        ));
        assert!(matches!(
            {
                let mut only_sps = Vec::new();
                only_sps.extend_from_slice(&[0, 0, 1]);
                only_sps.extend_from_slice(&SPS);
                avc_decoder_configuration_record(&only_sps)
            },
            Err(AnnexBError::MissingPps)
        ));
    }

    #[test]
    fn a_three_byte_start_code_is_accepted() {
        let mut unit = Vec::new();
        unit.extend_from_slice(&[0, 0, 1, 0x41, 0x7F, 0x00, 0x00, 0, 0, 1, 0x41, 0x7E]);
        let mut seen = Vec::new();
        for_each_nal(&unit, |_, payload| {
            seen.push(payload.to_vec());
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, [vec![0x41, 0x7F], vec![0x41, 0x7E]]);
    }
    #[test]
    fn empty_trailing_units_and_forbidden_headers_are_rejected() {
        for data in [
            &[0, 0, 1, 0, 0][..],
            &[0, 0, 1, 0x65, 0x55, 0, 0, 1][..],
            &[0, 0, 1, 0xe5, 0x55][..],
        ] {
            assert!(access_unit_size(data).is_err());
        }
    }
}
