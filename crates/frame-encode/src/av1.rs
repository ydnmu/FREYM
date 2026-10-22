//! Shared native AV1 Main 4:2:0 sequence and temporal-unit validation.
//! Bounded single-operating-point AV1 Main profile low-overhead framing.
//! Timing/model/scalable streams are refused; the native bridge disables them.
//! Matroska's av1C and random-access rules follow AOM AV1-ISOBMFF and V_AV1.

const MAX_CODEC_CONFIG_BYTES: usize = 64 * 1024;

/// Source-supplied HDR metadata OBUs. Some native drivers accept the metadata
/// request but omit the OBUs; keep AV1 elementary streams and replay starts
/// self-describing as well as preserving Matroska Colour.
pub fn preserve_hdr_metadata(data: &mut Vec<u8>, colour: crate::VideoColour) -> Result<(), String> {
    colour.validate().map_err(|e| e.message)?;
    let mut light_present = false;
    let mut mastering_present = false;
    obus(data, |kind, payload| {
        if kind == 5 {
            light_present |= payload.first() == Some(&1);
            mastering_present |= payload.first() == Some(&2);
        }
        Ok(())
    })?;
    let mut extra = Vec::new();
    if !light_present && let Some(light) = colour.content_light {
        let mut payload = vec![1];
        payload.extend_from_slice(&light.max_cll.to_be_bytes());
        payload.extend_from_slice(&light.max_fall.to_be_bytes());
        payload.push(0x80);
        extra.extend_from_slice(&[0x2a, payload.len() as u8]);
        extra.extend_from_slice(&payload);
    }
    if !mastering_present && let Some(m) = colour.mastering {
        if m.luminance_min > f64::from(u32::MAX) / 16384.0 {
            return Err("AV1 mastering minimum luminance exceeds its field".into());
        }
        let mut payload = vec![2];
        for value in [
            m.red.0, m.red.1, m.green.0, m.green.1, m.blue.0, m.blue.1, m.white.0, m.white.1,
        ] {
            let coded = ((value * 65536.0).round() as u32).min(u16::MAX.into()) as u16;
            payload.extend_from_slice(&coded.to_be_bytes());
        }
        payload.extend_from_slice(&((m.luminance_max * 256.0).round() as u32).to_be_bytes());
        payload.extend_from_slice(&((m.luminance_min * 16384.0).round() as u32).to_be_bytes());
        payload.push(0x80);
        extra.extend_from_slice(&[0x2a, payload.len() as u8]);
        extra.extend_from_slice(&payload);
    }
    if extra.is_empty() {
        return Ok(());
    }
    if data.len().saturating_add(extra.len()) > 8 * 1024 * 1024 {
        return Err("AV1 metadata exceeds its access-unit bound".into());
    }
    // Insert after delimiter/sequence and before the picture. Preserve all
    // original OBU bytes, size encodings and permitted extension headers.
    let mut at = 0;
    while at < data.len() {
        let header = data[at];
        if matches!((header >> 3) & 15, 3 | 6) {
            break;
        }
        at += 1 + usize::from(header & 4 != 0);
        let mut size = 0usize;
        let mut shift = 0;
        loop {
            let byte = data[at];
            at += 1;
            size |= usize::from(byte & 127) << shift;
            shift += 7;
            if byte < 128 {
                break;
            }
        }
        at += size;
    }
    data.splice(at..at, extra);
    Ok(())
}
#[derive(Clone, Debug)]
pub struct Sequence {
    pub width: u32,
    pub height: u32,
    pub depth: u8,
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    pub range: u8,
    reduced: bool,
    level: u8,
    tier: u8,
    chroma_position: u8,
}

struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}
impl<'a> Bits<'a> {
    fn read(&mut self, count: usize) -> Result<u32, String> {
        if count > 32
            || self
                .at
                .checked_add(count)
                .is_none_or(|end| end > self.data.len() * 8)
        {
            return Err("AV1 sequence/frame header is truncated".into());
        }
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1) | u32::from((self.data[self.at / 8] >> (7 - self.at % 8)) & 1);
            self.at += 1;
        }
        Ok(value)
    }
    fn flag(&mut self) -> Result<bool, String> {
        Ok(self.read(1)? != 0)
    }
}

fn obus<'a>(
    data: &'a [u8],
    mut visit: impl FnMut(u8, &'a [u8]) -> Result<(), String>,
) -> Result<(), String> {
    if data.is_empty() {
        return Err("AV1 OBU data is empty".into());
    }
    let mut at = 0;
    let mut count = 0;
    while at < data.len() {
        count += 1;
        if count > 4096 {
            return Err("AV1 temporal unit exceeds the OBU count bound".into());
        }
        let header = data[at];
        at += 1;
        if header & 0x81 != 0 || header & 2 == 0 {
            return Err("AV1 OBU header/reserved/size flag is invalid".into());
        }
        if header & 4 != 0 {
            let extension = *data.get(at).ok_or("AV1 extension is truncated")?;
            at += 1;
            if extension != 0 {
                return Err("AV1 multilayer/reserved extensions are unsupported".into());
            }
        }
        let mut size = 0u64;
        let mut complete = false;
        for index in 0..8 {
            let byte = *data.get(at).ok_or("AV1 OBU size is truncated")?;
            at += 1;
            size |= u64::from(byte & 0x7f) << (index * 7);
            if byte & 0x80 == 0 {
                complete = true;
                break;
            }
        }
        if !complete || size > 8 * 1024 * 1024 {
            return Err("AV1 OBU size exceeds its bound".into());
        }
        let end = at
            .checked_add(size as usize)
            .ok_or("AV1 OBU size overflows")?;
        let payload = data.get(at..end).ok_or("AV1 OBU payload is truncated")?;
        visit((header >> 3) & 15, payload)?;
        at = end;
    }
    Ok(())
}

fn parse_sequence(data: &[u8]) -> Result<Sequence, String> {
    let mut b = Bits { data, at: 0 };
    if b.read(3)? != 0 {
        return Err("Only AV1 Main profile is supported".into());
    }
    let still = b.flag()?;
    let reduced = b.flag()?;
    if reduced && !still {
        return Err("AV1 reduced header requires still_picture".into());
    }
    let (level, tier) = if reduced {
        (b.read(5)? as u8, 0)
    } else {
        if b.flag()? {
            return Err("AV1 decoder timing/model syntax is unsupported".into());
        }
        if b.flag()? || b.read(5)? != 0 || b.read(12)? != 0 {
            return Err("AV1 delayed/scalable operating points are unsupported".into());
        }
        let level = b.read(5)? as u8;
        let tier = if level > 7 { b.read(1)? as u8 } else { 0 };
        (level, tier)
    };
    let width_bits = b.read(4)? as usize + 1;
    let height_bits = b.read(4)? as usize + 1;
    let width = b.read(width_bits)? + 1;
    let height = b.read(height_bits)? + 1;
    if !reduced && b.flag()? {
        b.read(7)?;
    }
    b.read(3)?; // superblock, filter intra, intra edge
    if !reduced {
        b.read(4)?;
        let order = b.flag()?;
        if order {
            b.read(2)?;
        }
        let screen = if b.flag()? { true } else { b.flag()? };
        if screen && !b.flag()? {
            b.read(1)?;
        }
        if order {
            b.read(3)?;
        }
    }
    b.read(3)?; // superres, cdef, restoration
    let depth = if b.flag()? { 10 } else { 8 };
    if b.flag()? {
        return Err("AV1 monochrome is unsupported".into());
    }
    let (primaries, transfer, matrix) = if b.flag()? {
        (b.read(8)? as u8, b.read(8)? as u8, b.read(8)? as u8)
    } else {
        (2, 2, 2)
    };
    if (primaries, transfer, matrix) == (1, 13, 0) {
        return Err("AV1 RGB Main profile is unsupported".into());
    }
    let range = if b.flag()? { 2 } else { 1 };
    let chroma_position = b.read(2)? as u8;
    if chroma_position == 3 {
        return Err("AV1 chroma position is reserved".into());
    }
    b.read(2)?; // separate_uv_delta_q, film grain
    // Sequence OBU trailing_bits(): one then zero padding, never arbitrary tail.
    if !b.flag()? {
        return Err("AV1 sequence trailing bit is absent".into());
    }
    while b.at < data.len() * 8 {
        if b.flag()? {
            return Err("AV1 sequence has nonzero trailing padding".into());
        }
    }
    Ok(Sequence {
        width,
        height,
        depth,
        primaries,
        transfer,
        matrix,
        range,
        reduced,
        level,
        tier,
        chroma_position,
    })
}

pub fn sequence_payload(config: &[u8]) -> Result<&[u8], String> {
    let mut sequence = None;
    obus(config, |kind, payload| {
        match kind {
            1 if sequence.is_none() => {
                sequence = Some(payload);
            }
            2 if payload.is_empty() => {}
            5 | 15 => {}
            _ => {
                return Err(
                    "AV1 initialization must contain one sequence header and no frames".into(),
                );
            }
        }
        Ok(())
    })?;
    sequence.ok_or_else(|| "AV1 initialization has no sequence header".into())
}

pub fn configuration(config: &[u8]) -> Result<(Vec<u8>, Sequence), String> {
    if config.len() > MAX_CODEC_CONFIG_BYTES - 4 {
        return Err("AV1 initialization exceeds its configuration budget".into());
    }
    let sequence = parse_sequence(sequence_payload(config)?)?;
    let mut record = vec![
        0x81,
        sequence.level,
        (sequence.tier << 7)
            | (u8::from(sequence.depth == 10) << 6)
            | 0x0c
            | sequence.chroma_position,
        0,
    ];
    record.extend_from_slice(config);
    Ok((record, sequence))
}

pub fn sequence_payload_from_packet(data: &[u8]) -> Result<&[u8], String> {
    let mut sequence = None;
    obus(data, |kind, payload| {
        if kind == 1 {
            if sequence.is_some() {
                return Err("AV1 temporal unit repeats its sequence".into());
            }
            sequence = Some(payload);
        }
        Ok(())
    })?;
    sequence.ok_or_else(|| "AV1 temporal unit has no sequence header".into())
}

pub fn validate_configuration(record: &[u8]) -> Result<(), String> {
    if record.len() < 5
        || record.len() > MAX_CODEC_CONFIG_BYTES
        || record[0] != 0x81
        || record[3] != 0
    {
        return Err("AV1 configuration record has invalid bound/marker/version/delay".into());
    }
    let (expected, _) = configuration(&record[4..])?;
    if expected != record {
        return Err("AV1 configuration record disagrees with its sequence header".into());
    }
    Ok(())
}

/// One shown picture per temporal unit: either a Frame OBU or the native VA
/// Frame Header + Tile Group form. Multilayer/tile-list/show_existing streams
/// require a different accounting contract and remain refused.
pub fn validate_packet(data: &[u8], frozen: Option<&[u8]>) -> Result<bool, String> {
    let mut sequence = frozen.map(parse_sequence).transpose()?;
    let mut seen_sequence = false;
    let mut frame = None;
    let mut separate_header = false;
    let mut tile_group = false;
    obus(data, |kind, payload| {
        match kind {
            1 => {
                if frame.is_some() || seen_sequence {
                    return Err("AV1 temporal unit repeats or misorders its sequence".into());
                }
                if frozen.is_some_and(|expected| expected != payload) {
                    return Err("AV1 sequence changed; a new segment is required".into());
                }
                sequence = Some(parse_sequence(payload)?);
                seen_sequence = true;
            }
            6 if frame.is_none() => {
                frame = Some(payload);
            }
            3 if frame.is_none() => {
                frame = Some(payload);
                separate_header = true;
            }
            4 if separate_header && !tile_group && !payload.is_empty() => {
                tile_group = true;
            }
            2 if payload.is_empty() && frame.is_none() => {}
            5 | 15 => {}
            _ => {
                return Err("AV1 temporal unit requires one unsplit single-layer Frame OBU".into());
            }
        }
        Ok(())
    })?;
    if separate_header && !tile_group {
        return Err("AV1 frame header has no complete tile group".into());
    }
    let payload = frame.ok_or("AV1 temporal unit has no Frame OBU")?;
    if payload.is_empty() {
        return Err("AV1 Frame OBU is empty".into());
    }
    let s = sequence.ok_or("AV1 frame has no decoder sequence")?;
    let mut b = Bits {
        data: payload,
        at: 0,
    };
    let key = if s.reduced {
        true
    } else {
        if b.flag()? {
            return Err("AV1 show_existing_frame cannot account for a new source frame".into());
        }
        let key = b.read(2)? == 0;
        let shown = b.flag()?;
        if !shown {
            return Err("AV1 hidden frames require reordered accounting".into());
        }
        key
    };
    Ok(key && seen_sequence)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hdr_metadata_is_source_only_and_not_duplicated() {
        let fixture = include_bytes!("../../frame-mux/tests/fixtures/synthetic-main10.av1");
        let mut packet = fixture.to_vec();
        preserve_hdr_metadata(&mut packet, crate::VideoColour::BT2020_PQ_LIMITED).unwrap();
        assert_eq!(packet, fixture);
        let mut colour = crate::VideoColour::BT2020_PQ_LIMITED;
        colour.content_light = Some(crate::ContentLight {
            max_cll: 1000,
            max_fall: 400,
        });
        colour.mastering = Some(crate::MasteringDisplay {
            red: (0.68, 0.32),
            green: (0.265, 0.69),
            blue: (0.15, 0.06),
            white: (0.3127, 0.329),
            luminance_max: 1000.0,
            luminance_min: 0.005,
        });
        preserve_hdr_metadata(&mut packet, colour).unwrap();
        assert!(validate_packet(&packet, None).unwrap());
        let first = packet.clone();
        preserve_hdr_metadata(&mut packet, colour).unwrap();
        assert_eq!(packet, first);
    }
    #[test]
    fn separate_frame_header_requires_exactly_one_nonempty_tile_group() {
        let source = include_bytes!("../../frame-mux/tests/fixtures/synthetic-main10.av1");
        let mut unit = Vec::new();
        fn append(out: &mut Vec<u8>, kind: u8, data: &[u8]) {
            out.push((kind << 3) | 2);
            let mut len = data.len();
            loop {
                let byte = (len & 127) as u8;
                len >>= 7;
                out.push(byte | if len != 0 { 128 } else { 0 });
                if len == 0 {
                    break;
                }
            }
            out.extend_from_slice(data);
        }
        obus(source, |kind, data| {
            if kind == 6 {
                append(&mut unit, 3, &data[..1]);
                append(&mut unit, 4, &[0x55]);
            } else {
                append(&mut unit, kind, data);
            }
            Ok(())
        })
        .unwrap();
        assert!(validate_packet(&unit, None).unwrap());
        let mut extra = unit.clone();
        append(&mut extra, 4, &[0x55]);
        assert!(validate_packet(&extra, None).is_err());
        unit.truncate(unit.len() - 3);
        assert!(validate_packet(&unit, None).is_err());
    }

    #[test]
    fn malformed_or_unbounded_obus_are_rejected() {
        for bytes in [
            &[0x8a, 0][..],
            &[0x0a, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
            &[0x0a, 4, 1],
            &[0x08, 1],
        ] {
            assert!(configuration(bytes).is_err());
        }
        assert!(validate_packet(&[0x32, 1, 0x80], None).is_err());
    }
}
