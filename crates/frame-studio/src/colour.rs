//! High precision effect stage, used only when native YUV cannot carry the edit.
//! RGB samples retain the output transfer and primaries through the PNG mux.
use crate::*;

pub(crate) fn export_colour(timeline: &Timeline, info: &mut MediaInfo) -> StudioResult<()> {
    let rgb_source = info.pixel_format.starts_with("gbr");
    let active: Vec<_> = timeline
        .clips
        .iter()
        .map(|c| &info.segments[c.source_index])
        .collect();
    if timeline.output_colour == OutputColour::Preserve {
        if rgb_source && info.colour_range.is_none() {
            info.colour_range = Some("pc".into());
        }
        let hdr = active[0].hdr();
        if active
            .iter()
            .any(|s| s.hdr() != hdr || (hdr && s.colour_transfer != info.colour_transfer))
        {
            return Err("This timeline mixes HDR/SDR or PQ/HLG. Choose HDR PQ, HDR HLG or SDR tone map in Output colour before export.".into());
        }
        if active.iter().any(|s| s.hdr_metadata != info.hdr_metadata) {
            return Err("Segments have different mastering/light metadata. Choose an explicit output colour to create a new master without inherited mastering claims.".into());
        }
    } else {
        // An explicitly converted master must not inherit one segment's claims.
        info.hdr_metadata.clear();
        info.colour_primaries = Some(
            if timeline.output_colour == OutputColour::Sdr {
                "bt709"
            } else {
                "bt2020"
            }
            .into(),
        );
        info.colour_space = Some(
            if timeline.output_colour == OutputColour::Sdr {
                "bt709"
            } else {
                "bt2020nc"
            }
            .into(),
        );
        info.colour_transfer = Some(
            match timeline.output_colour {
                OutputColour::HdrPq => "smpte2084",
                OutputColour::HdrHlg => "arib-std-b67",
                _ => "bt709",
            }
            .into(),
        );
        info.colour_range = Some("tv".into());
        info.bit_depth = if timeline.output_colour == OutputColour::Sdr {
            info.bit_depth
        } else {
            10
        };
    }
    info.pixel_format = if info.bit_depth == 10 {
        "yuv420p10le"
    } else {
        "yuv420p"
    }
    .into();
    if info.colour_space.as_deref() == Some("gbr") || (rgb_source && info.colour_space.is_none()) {
        // Supported encoders store YUV; RGB->YUV changes the matrix, never the
        // gamut, transfer or precision. This is a necessary storage conversion.
        info.colour_space = Some(
            if info.colour_primaries.as_deref() == Some("bt2020") {
                "bt2020nc"
            } else {
                "bt709"
            }
            .into(),
        );
    }
    Ok(())
}

fn tags(info: &MediaInfo) -> StudioResult<(&str, &str, &str, &str)> {
    let matrix = info
        .colour_space
        .as_deref()
        .unwrap_or(if info.pixel_format.starts_with("gbr") {
            "gbr"
        } else {
            "bt709"
        });
    if info.hdr()
        && (info.colour_primaries.as_deref() != Some("bt2020")
            || !matches!(matrix, "bt2020nc" | "gbr"))
    {
        return Err(
            "HDR conversion requires declared BT.2020 YUV non-constant luminance or RGB colour."
                .into(),
        );
    }
    Ok((
        info.colour_primaries.as_deref().unwrap_or("bt709"),
        info.colour_transfer.as_deref().unwrap_or("bt709"),
        matrix,
        info.colour_range
            .as_deref()
            .unwrap_or(if matrix == "gbr" { "pc" } else { "tv" }),
    ))
}

/// No rate filter: every selected input PTS survives normalization.
pub(crate) fn normalize_filter(
    source: &MediaInfo,
    target: &MediaInfo,
    rgb: bool,
) -> StudioResult<String> {
    let (sp, st, sm, sr) = tags(source)?;
    let (p, t, m, r) = tags(target)?;
    let resize = if source.width != target.width || source.height != target.height {
        format!(",scale={}:{}:flags=bicubic", target.width, target.height)
    } else {
        String::new()
    };
    let changed = sp != p || st != t || sm != m || sr != r || source.bit_depth != target.bit_depth;
    let mut filter = if source.hdr() && !target.hdr() {
        // Explicit SDR policy only. Float linear-light tone mapping precedes quantization.
        format!(
            "zscale=pin={sp}:tin={st}:min={sm}:rin={sr}:t=linear:npl=203,format=gbrpf32le,zscale=p={p},tonemap=tonemap=mobius,zscale=t={t}:m={m}:r={r},format={}",
            target.pixel_format
        )
    } else if changed {
        format!(
            "zscale=pin={sp}:tin={st}:min={sm}:rin={sr}:p={p}:t={t}:m={m}:r={r}:npl=203,format={}",
            target.pixel_format
        )
    } else {
        format!("format={}", target.pixel_format)
    };
    filter.push_str(&resize);
    if rgb {
        filter.push_str(&format!(",zscale=pin={p}:tin={t}:min={m}:rin={r}:p={p}:t={t}:m=gbr:r=full,format=gbrp16le,format=rgb48le"));
    }
    Ok(filter)
}

pub(crate) fn rgb_output_filter(info: &MediaInfo) -> StudioResult<String> {
    let (p, t, m, r) = tags(info)?;
    Ok(format!(
        "format=gbrp16le,zscale=pin={p}:tin={t}:min=gbr:rin=full:p={p}:t={t}:m={m}:r={r},format={}",
        info.pixel_format
    ))
}

fn pq_decode(v: f64) -> f64 {
    let v = v.max(0.0).powf(1.0 / (2523.0 / 32.0));
    ((v - 3424.0 / 4096.0).max(0.0) / (2413.0 / 128.0 - 2392.0 / 128.0 * v).max(1e-10))
        .powf(1.0 / (2610.0 / 16384.0))
        * 10000.0
}
fn pq_encode(nits: f64) -> f64 {
    let v = (nits.max(0.0) / 10000.0).powf(2610.0 / 16384.0);
    ((3424.0 / 4096.0 + 2413.0 / 128.0 * v) / (1.0 + 2392.0 / 128.0 * v)).powf(2523.0 / 32.0)
}
fn hlg_decode(v: f64) -> f64 {
    if v <= 0.5 {
        v * v / 3.0
    } else {
        (((v - 0.55991073) / 0.17883277).exp() + 0.28466892) / 12.0
    }
}
fn hlg_encode(v: f64) -> f64 {
    if v <= 1.0 / 12.0 {
        (3.0 * v.max(0.0)).sqrt()
    } else {
        0.17883277 * (12.0 * v - 0.28466892).ln() + 0.55991073
    }
}
fn decode(rgb: [f64; 3], trc: &str) -> [f64; 3] {
    if trc == "smpte2084" {
        return rgb.map(pq_decode);
    }
    if trc == "arib-std-b67" {
        // BT.2100 reference display: 1000 nit peak, system gamma 1.2 OOTF.
        let scene = rgb.map(hlg_decode);
        let y = (scene[0] * 0.2627 + scene[1] * 0.6780 + scene[2] * 0.0593).max(0.0);
        return scene.map(|v| v * y.powf(0.2) * 1000.0);
    }
    rgb.map(|v| {
        if v < 0.081 {
            v / 4.5
        } else {
            ((v + 0.099) / 1.099).powf(1.0 / 0.45)
        }
    })
}
fn encode(rgb: [f64; 3], trc: &str) -> [f64; 3] {
    if trc == "smpte2084" {
        return rgb.map(pq_encode);
    }
    if trc == "arib-std-b67" {
        let display = rgb.map(|v| v.max(0.0) / 1000.0);
        let y = display[0] * 0.2627 + display[1] * 0.6780 + display[2] * 0.0593;
        let factor = y.max(1e-20).powf(-0.2 / 1.2);
        return display.map(|v| hlg_encode(v * factor));
    }
    rgb.map(|v| {
        if v < 0.018 {
            4.5 * v.max(0.0)
        } else {
            1.099 * v.powf(0.45) - 0.099
        }
    })
}

/// Shared cursor geometry, linear-light alpha, 16-bit transfer-encoded output.
pub(crate) fn render_rgb16(
    timeline: &Timeline,
    time: u64,
    info: &MediaInfo,
    bytes: &[u8],
) -> StudioResult<Vec<u8>> {
    let trc = info.colour_transfer.as_deref().unwrap_or("bt709");
    if !matches!(
        trc,
        "bt709" | "smpte170m" | "bt2020-10" | "bt2020-12" | "smpte2084" | "arib-std-b67"
    ) {
        return Err(format!(
            "High precision cursor effects do not support transfer {trc}. Choose an explicit output colour."
        ));
    }
    if !matches!(
        info.colour_primaries.as_deref(),
        Some("bt709" | "bt2020") | None
    ) {
        return Err("Cursor colour compositing supports BT.709/BT.2020 primaries. Choose an explicit output colour for this source.".into());
    }
    let w = info.width as usize;
    let h = info.height as usize;
    let mut source = Vec::with_capacity(w * h);
    for rgb in bytes.chunks_exact(6) {
        source.push(decode(
            std::array::from_fn(|c| {
                u16::from_le_bytes([rgb[c * 2], rgb[c * 2 + 1]]) as f64 / 65535.0
            }),
            trc,
        ));
    }
    let view = timeline.viewport(time);
    let mut output = vec![[0.0; 3]; w * h];
    for y in 0..h {
        for x in 0..w {
            let sx = ((view.left + (x as f64 + 0.5) / w as f64 * view.width) * w as f64 - 0.5)
                .clamp(0.0, (w - 1) as f64);
            let sy = ((view.top + (y as f64 + 0.5) / h as f64 * view.height) * h as f64 - 0.5)
                .clamp(0.0, (h - 1) as f64);
            let x0 = sx.floor() as usize;
            let y0 = sy.floor() as usize;
            let dx = sx - x0 as f64;
            let dy = sy - y0 as f64;
            for c in 0..3 {
                let a = source[y0 * w + x0][c] * (1.0 - dx)
                    + source[y0 * w + (x0 + 1).min(w - 1)][c] * dx;
                let b = source[(y0 + 1).min(h - 1) * w + x0][c] * (1.0 - dx)
                    + source[(y0 + 1).min(h - 1) * w + (x0 + 1).min(w - 1)][c] * dx;
                output[y * w + x][c] = a * (1.0 - dy) + b * dy;
            }
        }
    }
    crate::render::paint_cursor(
        timeline,
        time,
        info.width,
        info.height,
        |at, colour, alpha| {
            let linear = colour.map(|v| {
                let v = v / 255.0;
                if v <= 0.04045 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            });
            let rgb = if info.colour_primaries.as_deref() == Some("bt2020") {
                [
                    0.627404 * linear[0] + 0.329283 * linear[1] + 0.043313 * linear[2],
                    0.069097 * linear[0] + 0.919540 * linear[1] + 0.011362 * linear[2],
                    0.016391 * linear[0] + 0.088013 * linear[1] + 0.895595 * linear[2],
                ]
            } else {
                linear
            };
            let white = if info.hdr() { 203.0 } else { 1.0 };
            for c in 0..3 {
                output[at][c] = output[at][c] * (1.0 - alpha) + rgb[c] * white * alpha;
            }
        },
    );
    let mut result = Vec::with_capacity(bytes.len());
    for pixel in output {
        for value in encode(pixel, trc) {
            result.extend_from_slice(
                &((value.clamp(0.0, 1.0) * 65535.0).round() as u16).to_be_bytes(),
            );
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hdr_light_round_trip_and_reference_white() {
        for nits in [0.0, 0.1, 203.0, 1000.0, 10000.0] {
            assert!((pq_decode(pq_encode(nits)) - nits).abs() < 0.001);
        }
        for rgb in [[203.0; 3], [1000.0; 3], [400.0, 50.0, 20.0]] {
            let round = decode(encode(rgb, "arib-std-b67"), "arib-std-b67");
            for c in 0..3 {
                assert!((round[c] - rgb[c]).abs() < 0.001);
            }
        }
    }
}
