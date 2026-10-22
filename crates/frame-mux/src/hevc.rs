//! HEVC Annex-B to Matroska framing and HEVCDecoderConfigurationRecord (ISO/IEC 14496-15).
//! Parameter sets and access units are copied byte for byte apart from start codes.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HevcError {
    Empty,
    StartCode,
    EmptyNal,
    InvalidNal,
    TooLarge,
    MissingParameterSet(&'static str),
    ShortSps,
    UnsupportedSps,
}

/// Common HEVC header checks for Annex-B input and recovered hvcC/packet data.
fn nal_type(nal: &[u8]) -> Result<u8, HevcError> {
    if nal.len() < 2 {
        return Err(HevcError::EmptyNal);
    }
    // forbidden_zero_bit=0, nuh_temporal_id_plus1 != 0.
    if nal[0] & 0x80 != 0 || nal[1] & 7 == 0 {
        return Err(HevcError::InvalidNal);
    }
    Ok((nal[0] >> 1) & 0x3f)
}

impl fmt::Display for HevcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Empty => "empty HEVC Annex-B data",
                Self::StartCode => "HEVC data is not start-code delimited",
                Self::EmptyNal => "an HEVC NAL has no payload",
                Self::InvalidNal => "an HEVC NAL has an invalid header",
                Self::TooLarge => "an HEVC NAL or parameter set exceeds its length field",
                Self::MissingParameterSet(kind) => return write!(f, "missing HEVC {kind}"),
                Self::ShortSps => "HEVC SPS is too short for profile-tier-level",
                Self::UnsupportedSps =>
                    "HEVC SPS has unsupported or malformed geometry/chroma/bit-depth",
            }
        )
    }
}

fn prefix(data: &[u8], at: usize) -> Option<usize> {
    if data.get(at..at + 4) == Some(&[0, 0, 0, 1]) {
        Some(at + 4)
    } else if data.get(at..at + 3) == Some(&[0, 0, 1]) {
        Some(at + 3)
    } else {
        None
    }
}

fn next_prefix(data: &[u8], from: usize) -> Option<(usize, usize)> {
    (from..data.len()).find_map(|i| prefix(data, i).map(|end| (i, end)))
}

pub(crate) fn for_each_nal<'a>(
    data: &'a [u8],
    mut visit: impl FnMut(u8, &'a [u8]) -> Result<(), HevcError>,
) -> Result<(), HevcError> {
    if data.is_empty() {
        return Err(HevcError::Empty);
    }
    let mut start = prefix(data, 0).ok_or(HevcError::StartCode)?;
    loop {
        let next = next_prefix(data, start);
        let mut end = next.map_or(data.len(), |(i, _)| i);
        while end > start && data[end - 1] == 0 {
            end -= 1;
        }
        let nal = data.get(start..end).ok_or(HevcError::EmptyNal)?;
        visit(nal_type(nal)?, nal)?;
        match next {
            Some((_, after)) => start = after,
            None => break,
        }
    }
    Ok(())
}

pub(crate) fn access_unit_size(data: &[u8]) -> Result<usize, HevcError> {
    let mut size = 0usize;
    for_each_nal(data, |_, nal| {
        u32::try_from(nal.len()).map_err(|_| HevcError::TooLarge)?;
        size = size.checked_add(4 + nal.len()).ok_or(HevcError::TooLarge)?;
        Ok(())
    })?;
    Ok(size)
}

pub(crate) fn length_prefixed_access_unit(data: &[u8], out: &mut Vec<u8>) -> Result<(), HevcError> {
    let size = access_unit_size(data)?;
    out.clear();
    out.reserve_exact(size);
    for_each_nal(data, |_, nal| {
        let len = u32::try_from(nal.len()).map_err(|_| HevcError::TooLarge)?;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(nal);
        Ok(())
    })
}

pub(crate) fn decoder_configuration_record(config: &[u8]) -> Result<Vec<u8>, HevcError> {
    let mut sets: [Vec<&[u8]>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for_each_nal(config, |kind, nal| {
        if (32..=34).contains(&kind) {
            sets[usize::from(kind - 32)].push(nal);
        }
        Ok(())
    })?;
    for (index, name) in ["VPS", "SPS", "PPS"].into_iter().enumerate() {
        if sets[index].is_empty() {
            return Err(HevcError::MissingParameterSet(name));
        }
    }
    // SPS: two-byte NAL header; four-bit VPS id, three-bit max sublayers minus one,
    // one-bit temporal nesting; then the 12-byte general profile-tier-level record.
    let sps = sps_fields(sets[1][0])?;
    let mut record = Vec::with_capacity(64);
    record.push(1);
    record.extend_from_slice(&sps.ptl);
    record.extend_from_slice(&[
        0xf0,
        0,
        0xfc,
        0xfc | sps.chroma,
        0xf8 | sps.luma_minus8,
        0xf8 | sps.chroma_minus8,
        0,
        0,
    ]);
    record.push((sps.sublayers << 3) | (sps.nested << 2) | 3);
    record.push(3);
    for (index, list) in sets.iter().enumerate() {
        let count = u16::try_from(list.len()).map_err(|_| HevcError::TooLarge)?;
        record.push(0x80 | (index as u8 + 32));
        record.extend_from_slice(&count.to_be_bytes());
        for nal in list {
            let len = u16::try_from(nal.len()).map_err(|_| HevcError::TooLarge)?;
            record.extend_from_slice(&len.to_be_bytes());
            record.extend_from_slice(nal);
        }
    }
    Ok(record)
}

struct SpsFields {
    ptl: [u8; 12],
    sublayers: u8,
    nested: u8,
    chroma: u8,
    luma_minus8: u8,
    chroma_minus8: u8,
}

/// Parse RBSP, removing emulation-prevention bytes. hvcC describes the actual
/// coded profile/depth rather than assuming Main10 or copying escaped bytes.
fn sps_fields(nal: &[u8]) -> Result<SpsFields, HevcError> {
    let mut rbsp = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &byte in nal.get(2..).ok_or(HevcError::ShortSps)? {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        rbsp.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    let header = *rbsp.first().ok_or(HevcError::ShortSps)?;
    let sublayers = (header >> 1) & 7;
    if sublayers > 6 {
        return Err(HevcError::UnsupportedSps);
    }
    let ptl: [u8; 12] = rbsp
        .get(1..13)
        .ok_or(HevcError::ShortSps)?
        .try_into()
        .unwrap();
    let mut bits = SpsBits {
        data: &rbsp,
        at: 104,
    };
    let mut flags = Vec::new();
    for _ in 0..sublayers {
        flags.push((bits.read(1)?, bits.read(1)?));
    }
    if sublayers > 0 {
        bits.read(usize::from(8 - sublayers) * 2)?;
    }
    for (profile, level) in flags {
        if profile != 0 {
            bits.read(88)?;
        }
        if level != 0 {
            bits.read(8)?;
        }
    }
    bits.ue()?;
    let chroma = bits.ue()?;
    if chroma != 1 {
        return Err(HevcError::UnsupportedSps);
    }
    if bits.ue()? == 0 || bits.ue()? == 0 {
        return Err(HevcError::UnsupportedSps);
    }
    if bits.read(1)? != 0 {
        for _ in 0..4 {
            bits.ue()?;
        }
    }
    let luma = bits.ue()?;
    let chroma_depth = bits.ue()?;
    if !matches!(luma, 0 | 2) || chroma_depth != luma {
        return Err(HevcError::UnsupportedSps);
    }
    Ok(SpsFields {
        ptl,
        sublayers: sublayers + 1,
        nested: header & 1,
        chroma: chroma as u8,
        luma_minus8: luma as u8,
        chroma_minus8: chroma_depth as u8,
    })
}

struct SpsBits<'a> {
    data: &'a [u8],
    at: usize,
}
impl SpsBits<'_> {
    fn read(&mut self, count: usize) -> Result<u32, HevcError> {
        if self
            .at
            .checked_add(count)
            .is_none_or(|end| end > self.data.len() * 8)
        {
            return Err(HevcError::ShortSps);
        }
        let mut value = 0u32;
        for _ in 0..count {
            value = (value << 1) | u32::from((self.data[self.at / 8] >> (7 - self.at % 8)) & 1);
            self.at += 1;
        }
        Ok(value)
    }
    fn ue(&mut self) -> Result<u32, HevcError> {
        let mut zeros = 0;
        while self.read(1)? == 0 {
            zeros += 1;
            if zeros > 30 {
                return Err(HevcError::UnsupportedSps);
            }
        }
        Ok(((1u32 << zeros) - 1) + self.read(zeros)?)
    }
}

/// Validates the hvcC profile emitted by this writer, without interpreting AVC
/// fields as HEVC fields or trusting the container's codec label.
pub(crate) fn validate_decoder_configuration_record(record: &[u8]) -> Result<(), String> {
    if record.len() < 23 || record[0] != 1 {
        return Err("HEVC configuration needs a version 1, 23-byte hvcC header".into());
    }
    if record[21] & 3 != 3 {
        return Err("HEVC recovery requires four-byte NAL length prefixes".into());
    }
    let mut position = 23usize;
    let mut sets = [false; 3];
    for _ in 0..record[22] {
        let array = record
            .get(position..position + 3)
            .ok_or_else(|| "HEVC parameter-set array is cut short".to_string())?;
        if array[0] & 0x40 != 0 {
            return Err("HEVC parameter-set array has a reserved bit set".into());
        }
        let kind = array[0] & 0x3f;
        let count = u16::from_be_bytes([array[1], array[2]]);
        position += 3;
        for _ in 0..count {
            let length = record
                .get(position..position + 2)
                .ok_or_else(|| "HEVC parameter-set length is cut short".to_string())?;
            let length = usize::from(u16::from_be_bytes([length[0], length[1]]));
            position += 2;
            let nal = record
                .get(position..position + length)
                .ok_or_else(|| "HEVC parameter set is cut short".to_string())?;
            let actual = nal_type(nal).map_err(|error| error.to_string())?;
            if actual != kind || nal[0] & 1 != 0 || nal[1] >> 3 != 0 {
                return Err(
                    "HEVC parameter-set array disagrees with its single-layer NAL header".into(),
                );
            }
            if (32..=34).contains(&kind) {
                let minimum = if kind == 33 { 15 } else { 3 };
                if nal.len() < minimum {
                    return Err("HEVC VPS/SPS/PPS initialization is cut short".into());
                }
                sets[usize::from(kind - 32)] = true;
            }
            position += length;
        }
    }
    if position != record.len() {
        return Err("HEVC configuration has trailing bytes outside its arrays".into());
    }
    for (present, name) in sets.into_iter().zip(["VPS", "SPS", "PPS"]) {
        if !present {
            return Err(format!("HEVC configuration has no {name}"));
        }
    }
    Ok(())
}

/// Returns whether one length-prefixed HEVC access unit contains a first IDR/BLA
/// slice. CRA is deliberately not treated as an independent recovery point: its
/// leading-picture rules need a separate recovery policy.
pub(crate) fn validate_recovered_access_unit(payload: &[u8]) -> Result<bool, String> {
    let mut position = 0usize;
    let mut has_vcl = false;
    let mut independent_first_slice = false;
    let mut dependent_vcl = false;
    while position < payload.len() {
        let bytes = payload
            .get(position..position + 4)
            .ok_or_else(|| "HEVC NAL length prefix is cut short".to_string())?;
        let length = u32::from_be_bytes(bytes.try_into().expect("four length bytes")) as usize;
        position += 4;
        let end = position
            .checked_add(length)
            .ok_or_else(|| "HEVC NAL length overflows".to_string())?;
        let nal = payload
            .get(position..end)
            .ok_or_else(|| "HEVC NAL payload is cut short".to_string())?;
        let kind = nal_type(nal).map_err(|error| error.to_string())?;
        if nal[0] & 1 != 0 || nal[1] >> 3 != 0 {
            return Err("HEVC recovery supports single-layer pictures only".into());
        }
        if kind <= 31 {
            has_vcl = true;
            let first_slice = nal
                .get(2)
                .ok_or_else(|| "HEVC VCL slice header is cut short".to_string())?
                & 0x80
                != 0;
            if (16..=20).contains(&kind) {
                independent_first_slice |= first_slice;
            } else {
                dependent_vcl = true;
            }
        }
        position = end;
    }
    if !has_vcl {
        return Err("HEVC access unit has no coded picture".into());
    }
    if independent_first_slice && dependent_vcl {
        return Err("HEVC access unit mixes independent and dependent pictures".into());
    }
    Ok(independent_first_slice)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAIN10: &[u8] = include_bytes!("../tests/fixtures/synthetic-main10.hevc");

    #[test]
    fn hvcc_initialization_checks_lengths_headers_and_all_parameter_sets() {
        let config = decoder_configuration_record(MAIN10).unwrap();
        validate_decoder_configuration_record(&config).unwrap();
        for end in 0..config.len() {
            assert!(validate_decoder_configuration_record(&config[..end]).is_err());
        }
        let mut wrong_version = config.clone();
        wrong_version[0] = 2;
        assert!(validate_decoder_configuration_record(&wrong_version).is_err());
        let mut wrong_lengths = config.clone();
        wrong_lengths[21] &= !3;
        assert!(validate_decoder_configuration_record(&wrong_lengths).is_err());
        let mut missing_pps = config.clone();
        // Declare two arrays and omit the PPS array entirely.
        let mut at = 23;
        for _ in 0..2 {
            let count = u16::from_be_bytes([config[at + 1], config[at + 2]]);
            at += 3;
            for _ in 0..count {
                let length = u16::from_be_bytes([config[at], config[at + 1]]) as usize;
                at += 2 + length;
            }
        }
        missing_pps[22] = 2;
        missing_pps.truncate(at);
        assert!(
            validate_decoder_configuration_record(&missing_pps)
                .unwrap_err()
                .contains("PPS")
        );
        let mut wrong_header = config;
        wrong_header[28] |= 0x80;
        assert!(validate_decoder_configuration_record(&wrong_header).is_err());
    }

    fn payload(kind: u8, first_slice: bool) -> Vec<u8> {
        vec![
            0,
            0,
            0,
            3,
            kind << 1,
            1,
            if first_slice { 0x80 } else { 0x40 },
        ]
    }

    #[test]
    fn independent_recovery_points_need_an_actual_idr_or_bla_first_slice() {
        let mut fixture = Vec::new();
        length_prefixed_access_unit(MAIN10, &mut fixture).unwrap();
        assert!(validate_recovered_access_unit(&fixture).unwrap());
        for kind in 16..=20 {
            assert!(validate_recovered_access_unit(&payload(kind, true)).unwrap());
            assert!(!validate_recovered_access_unit(&payload(kind, false)).unwrap());
        }
        assert!(
            !validate_recovered_access_unit(&payload(21, true)).unwrap(),
            "CRA is conservative"
        );
        assert!(!validate_recovered_access_unit(&payload(1, true)).unwrap());
        assert!(validate_recovered_access_unit(&[0, 0, 0, 2, 38, 1]).is_err());
        assert!(validate_recovered_access_unit(&[0, 0, 0, 3, 38, 0, 0x80]).is_err());
        assert!(validate_recovered_access_unit(&[0, 0, 0, 9, 38, 1, 0x80]).is_err());
        let mut mixed = payload(19, true);
        mixed.extend(payload(1, true));
        assert!(validate_recovered_access_unit(&mixed).is_err());
    }
}
