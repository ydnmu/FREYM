//! Packet preservation inside Studio's existing FFmpeg/Matroska export path.
//! The recorder's non-reordering mux contract is deliberately left unchanged.
use crate::assembly::{concat_file, run};
use crate::media::probe_output;
use crate::render::{Workspace, check_cancel};
use crate::*;
use serde_json::Value;
use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

const PRECISION_NS: i64 = 1_000_000; // Existing final Matroska mux precision.
fn seconds(ns: u64) -> String {
    format!("{}.{:09}", ns / 1_000_000_000, ns % 1_000_000_000)
}
fn time(v: &Value) -> StudioResult<i64> {
    let s = v.as_str().ok_or("Missing packet timestamp.")?;
    let negative = s.starts_with('-');
    let s = s.trim_start_matches('-');
    let (whole, fraction) = s.split_once('.').unwrap_or((s, ""));
    if fraction.len() > 9 || !fraction.bytes().all(|c| c.is_ascii_digit()) {
        return Err("Unsupported packet timestamp precision.".into());
    }
    let ns = whole
        .parse::<i64>()
        .map_err(|e| e.to_string())?
        .checked_mul(1_000_000_000)
        .and_then(|n| n.checked_add(format!("{fraction:0<9}").parse::<i64>().ok()?))
        .ok_or("Packet timestamp overflow.")?;
    Ok(if negative { -ns } else { ns })
}
fn hex_dump(v: &Value) -> StudioResult<Vec<u8>> {
    let mut bytes = Vec::new();
    for line in v.as_str().ok_or("Missing codec bytes.")?.lines() {
        let Some((_, rest)) = line.split_once(':') else {
            continue;
        };
        let hex = rest
            .trim_start()
            .split("  ")
            .next()
            .unwrap_or("")
            .replace(' ', "");
        if hex.len() % 2 != 0 {
            return Err("Invalid codec byte dump.".into());
        }
        for pair in hex.as_bytes().chunks_exact(2) {
            bytes.push(
                u8::from_str_radix(std::str::from_utf8(pair).map_err(|e| e.to_string())?, 16)
                    .map_err(|e| e.to_string())?,
            );
        }
    }
    Ok(bytes)
}
#[derive(Clone, Debug)]
struct Packet {
    pts: i64,
    dts: Option<i64>,
    duration: i64,
    key: bool,
    hash: String,
    side: Value,
}
struct PacketMedia {
    path: PathBuf,
    stream: Value,
    packets: Vec<Packet>,
}
impl PacketMedia {
    fn configuration(&self) -> Value {
        // No container-specific stream index, time base, duration or encoder tag.
        // Extradata bytes (not just profile/codec names) must match across a join.
        let mut result = serde_json::Map::new();
        for key in [
            "codec_name",
            "profile",
            "level",
            "width",
            "height",
            "pix_fmt",
            "sample_aspect_ratio",
            "field_order",
            "color_range",
            "color_space",
            "color_transfer",
            "color_primaries",
            "chroma_location",
            "extradata",
            "sample_rate",
            "channels",
            "channel_layout",
            "bits_per_raw_sample",
        ] {
            if let Some(value) = self.stream.get(key) {
                result.insert(key.into(), value.clone());
            }
        }
        Value::Object(result)
    }
    fn reordered(&self) -> bool {
        self.stream["has_b_frames"].as_u64().unwrap_or(0) > 0
            || self.packets.windows(2).any(|p| p[0].pts >= p[1].pts)
            || self
                .packets
                .iter()
                .any(|p| p.dts.is_some_and(|d| d != p.pts))
    }
    fn range(&self, start: u64, end: u64) -> StudioResult<Vec<Packet>> {
        let selected: Vec<_> = self
            .packets
            .iter()
            .enumerate()
            .filter(|(_, p)| p.pts >= start as i64 && p.pts < end as i64)
            .collect();
        if selected.is_empty() || selected.windows(2).any(|p| p[1].0 != p[0].0 + 1) {
            return Err("Cut would break packet decode order or reference dependencies.".into());
        }
        Ok(selected.into_iter().map(|(_, p)| p.clone()).collect())
    }
}
fn check_packets(actual: &[Packet], expected: &[Packet], offset: i64) -> StudioResult<()> {
    if actual.len() != expected.len() {
        return Err("Packet copy changed the selected packet count.".into());
    }
    for (a, e) in actual.iter().zip(expected) {
        if a.hash != e.hash
            || a.key != e.key
            || a.side != e.side
            || (a.pts - (e.pts + offset)).abs() > PRECISION_NS
            || (a.duration > 0 && e.duration > 0 && (a.duration - e.duration).abs() > PRECISION_NS)
            || matches!((a.dts, e.dts), (Some(a), Some(e)) if (a - (e + offset)).abs() > PRECISION_NS)
        {
            return Err(
                "Packet copy changed payload, timing, dependencies or audio trim metadata.".into(),
            );
        }
    }
    Ok(())
}
impl LocalTools {
    fn packet_media(&self, path: &Path, select: &str) -> StudioResult<PacketMedia> {
        let mut cmd = command(&self.ffprobe);
        cmd.args([
            "-v",
            "error",
            "-select_streams",
            select,
            "-show_streams",
            "-show_data",
            "-of",
            "json",
        ])
        .arg(path);
        let streams: Value =
            serde_json::from_slice(&probe_output(cmd)?).map_err(|e| e.to_string())?;
        let stream = streams["streams"]
            .as_array()
            .filter(|s| s.len() == 1)
            .ok_or("Packet export requires one selected stream.")?[0]
            .clone();
        let mut cmd = command(&self.ffprobe);
        cmd.args([
            "-v",
            "error",
            "-select_streams",
            select,
            "-show_packets",
            "-show_data_hash",
            "sha256",
            "-show_entries",
            "packet=pts_time,dts_time,duration_time,flags,data_hash,side_data_list",
            "-of",
            "json",
        ])
        .arg(path);
        let data: Value = serde_json::from_slice(&probe_output(cmd)?).map_err(|e| e.to_string())?;
        let packets = data["packets"]
            .as_array()
            .ok_or("Missing encoded packet timeline.")?
            .iter()
            .map(|p| {
                Ok(Packet {
                    pts: time(&p["pts_time"])?,
                    dts: p.get("dts_time").map(time).transpose()?,
                    duration: p.get("duration_time").map(time).transpose()?.unwrap_or(0),
                    key: p["flags"].as_str().is_some_and(|f| f.contains('K')),
                    hash: p["data_hash"]
                        .as_str()
                        .ok_or("Missing packet preservation hash.")?
                        .into(),
                    side: p["side_data_list"].clone(),
                })
            })
            .collect::<StudioResult<Vec<_>>>()?;
        Ok(PacketMedia {
            path: path.to_path_buf(),
            stream,
            packets,
        })
    }
    fn independent(&self, path: &Path, media: &PacketMedia, pts: u64) -> StudioResult<bool> {
        // Key flags also mark CRA/open-GOP pictures. Inspect the actual coded unit
        // with the existing mux/replay gate. The bounded read may seek earlier.
        let mut cmd = command(&self.ffprobe);
        cmd.args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-read_intervals",
            &format!("{}%+#64", seconds(pts)),
            "-show_packets",
            "-show_data",
            "-show_entries",
            "packet=pts_time,data",
            "-of",
            "json",
        ])
        .arg(path);
        let data: Value = serde_json::from_slice(&probe_output(cmd)?).map_err(|e| e.to_string())?;
        let Some(row) = data["packets"].as_array().and_then(|rows| {
            rows.iter()
                .find(|p| time(&p["pts_time"]).ok() == Some(pts as i64))
        }) else {
            return Ok(false);
        };
        let packet = hex_dump(&row["data"])?;
        let config = hex_dump(&media.stream["extradata"])?;
        use frame_encode::BitstreamFormat::*;
        let (format, length) = match media.stream["codec_name"].as_str() {
            Some("h264") if config.first() == Some(&1) && config.len() > 4 => {
                (H264AnnexB, (config[4] & 3) + 1)
            }
            Some("hevc") if config.first() == Some(&1) && config.len() > 21 => {
                (HevcAnnexB, (config[21] & 3) + 1)
            }
            Some("av1") if config.len() > 4 => {
                return Ok(
                    frame_mux::validate_av1_access_unit(&packet, Some(&config[4..]))
                        .unwrap_or(false),
                );
            }
            _ => return Ok(false),
        };
        let mut annexb = Vec::with_capacity(packet.len());
        let mut at = 0;
        while at < packet.len() {
            let Some(prefix) = packet.get(at..at + length as usize) else {
                return Ok(false);
            };
            let size = prefix.iter().fold(0usize, |n, b| (n << 8) | *b as usize);
            at += length as usize;
            let Some(nal) = packet.get(at..at.saturating_add(size)) else {
                return Ok(false);
            };
            if size == 0 {
                return Ok(false);
            }
            annexb.extend_from_slice(&[0, 0, 0, 1]);
            annexb.extend_from_slice(nal);
            at += size;
        }
        Ok(frame_mux::independently_decodable_start(format, &annexb))
    }
    fn copy_video(
        &self,
        media: &PacketMedia,
        range: std::ops::Range<u64>,
        output: &Path,
        cancel: &AtomicBool,
    ) -> StudioResult<Vec<Packet>> {
        let std::ops::Range { start, end } = range;
        let source = &media.path;
        let scratch = output.parent().ok_or("Export needs a directory.")?;
        let expected = media.range(start, end)?;
        let mut cmd = command(&self.ffmpeg);
        cmd.args(["-v", "error", "-xerror", "-nostdin", "-n", "-copyts"]);
        let shift;
        if start == 0 {
            // Input -ss 0 can remove a container's nonzero initial offset.
            cmd.arg("-i").arg(source);
            shift = 0;
        } else if media.reordered() {
            // Demuxer input seeking can land on the preceding GOP's DTS. Scan
            // packets to the validated RAP instead; retain decode order and PTS.
            let seek = expected[0].dts.unwrap_or(0).max(0) as u64;
            cmd.arg("-i").arg(source).args(["-ss", &seconds(seek)]);
            // Output -ss subtracts its DTS threshold. Restore the presentation
            // origin explicitly; the reference chain keeps its PTS-DTS offsets.
            shift = seek as i64 - start as i64;
        } else {
            cmd.args(["-seek_timestamp", "1", "-ss", &seconds(start), "-i"])
                .arg(source);
            shift = -(start as i64);
        }
        let shift = format!(
            "{}{value}",
            if shift < 0 { "-" } else { "" },
            value = seconds(shift.unsigned_abs())
        );
        cmd.args([
            "-map",
            "0:v:0",
            "-an",
            "-sn",
            "-dn",
            "-c:v",
            "copy",
            "-frames:v",
            &expected.len().to_string(),
            "-output_ts_offset",
            &shift,
            "-avoid_negative_ts",
            "disabled",
            "-f",
            "matroska",
        ])
        .arg(output);
        run(cmd, &scratch.join("packet-copy.log"), cancel)?;
        let copied = self.packet_media(output, "v:0")?;
        if copied.configuration() != media.configuration() {
            return Err("Remux changed video decoder configuration.".into());
        }
        check_packets(&copied.packets, &expected, -(start as i64))?;
        Ok(copied.packets)
    }
    fn encode_boundary(
        &self,
        media: &PacketMedia,
        info: &MediaInfo,
        range: std::ops::Range<u64>,
        profile: ExportProfile,
        output: &Path,
        cancel: &AtomicBool,
    ) -> StudioResult<Vec<Packet>> {
        let std::ops::Range { start, end } = range;
        let source = &media.path;
        let scratch = output.parent().ok_or("Export needs a directory.")?;
        // Matching reordered encoder state cannot be reconstructed from has_b_frames.
        if media.reordered() {
            return Err("Compatible boundary encoding for reordered video is unavailable.".into());
        }
        self.require_encoder(profile, info.bit_depth)?;
        let first = info
            .frames
            .partition_point(|(t, _)| t.0 <= start)
            .saturating_sub(1);
        let last = info.frames.partition_point(|(t, _)| t.0 < end);
        let mut decode_start = 0;
        for p in media
            .packets
            .iter()
            .rev()
            .filter(|p| p.key && p.pts >= 0 && p.pts <= info.frames[first].0.0 as i64)
        {
            check_cancel(cancel)?;
            if self.independent(source, media, p.pts as u64)? {
                decode_start = p.pts as u64;
                break;
            }
        }
        let mut cmd = command(&self.ffmpeg);
        cmd.args([
            "-v",
            "error",
            "-xerror",
            "-nostdin",
            "-n",
            "-copyts",
            "-noautorotate",
            "-seek_timestamp",
            "1",
            "-ss",
            &seconds(decode_start),
            "-i",
        ])
        .arg(source)
        .args([
            "-map",
            "0:v:0",
            "-an",
            "-sn",
            "-dn",
            "-vf",
            &format!(
                "trim=start={}:end={},setpts=max(PTS-{}/TB\\,0)",
                seconds(info.frames[first].0.0),
                seconds(end),
                seconds(start)
            ),
            "-frames:v",
            &(last - first).to_string(),
            "-fps_mode:v",
            "passthrough",
            "-enc_time_base:v",
            &format!("{}:{}", info.rate_den, info.rate_num),
            "-threads:v",
            "2",
            "-bf",
            "0",
            "-avoid_negative_ts",
            "disabled",
        ]);
        self.configure_video_encoder(&mut cmd, profile, info, true)?;
        if matches!(profile, ExportProfile::Hevc { .. }) {
            // Hardware HEVC often has different VPS/SPS. Exact comparison below
            // rejects that combination rather than accepting a nominal profile match.
            let args: Vec<_> = cmd.get_args().collect();
            let inherited = args
                .windows(2)
                .find(|a| a[0] == "-x265-params")
                .map(|a| a[1].to_string_lossy().into_owned())
                .unwrap_or_default();
            cmd.args([
                "-x265-params",
                &format!("{inherited}:bframes=0:pools=none:frame-threads=2"),
            ]);
        }
        cmd.args(["-f", "matroska"]).arg(output);
        run(cmd, &scratch.join("boundary-encode.log"), cancel)?;
        let encoded = self.packet_media(output, "v:0")?;
        if encoded.configuration() != media.configuration() {
            return Err("Boundary encoder extradata/profile/colour differs from the source; full rendering is required.".into());
        }
        if encoded.packets.len() != last - first
            || encoded.packets.is_empty()
            || !self.independent(output, &encoded, encoded.packets[0].pts.max(0) as u64)?
        {
            return Err(
                "Boundary encoder did not produce an independent frame-accurate region.".into(),
            );
        }
        for (p, (t, _)) in encoded.packets.iter().zip(&info.frames[first..last]) {
            if (p.pts - t.0.saturating_sub(start) as i64).abs() > PRECISION_NS {
                return Err("Boundary encoder changed edit timing.".into());
            }
        }
        Ok(encoded.packets)
    }
    pub(crate) fn try_packet_export(
        &self,
        timeline: &Timeline,
        info: &MediaInfo,
        output: &Path,
        profile: ExportProfile,
        cancel: &AtomicBool,
        progress: &AtomicU64,
    ) -> StudioResult<Option<ExportReport>> {
        if timeline.output_colour != OutputColour::Preserve
            || timeline.zooms.iter().any(|z| z.enabled && z.scale != 1.0)
            || timeline.cursor.enabled
            || profile == ExportProfile::Lossless
        {
            return Ok(None);
        }
        let codec = match profile {
            ExportProfile::H264 { .. } => "h264",
            ExportProfile::Hevc { .. } => "hevc",
            ExportProfile::Av1 { .. } => "av1",
            _ => return Ok(None),
        };
        let mut sources = std::collections::BTreeMap::new();
        for clip in &timeline.clips {
            check_cancel(cancel)?;
            if sources.contains_key(&clip.source_index) {
                continue;
            }
            let media = self.packet_media(&timeline.sources[clip.source_index].path, "v:0")?;
            let s = &info.segments[clip.source_index];
            let mut packet_pts: Vec<_> = media.packets.iter().map(|p| p.pts).collect();
            packet_pts.sort_unstable();
            if packet_pts.len() != s.frames.len()
                || packet_pts
                    .iter()
                    .zip(&s.frames)
                    .any(|(p, (t, _))| *p != t.0 as i64)
            {
                return Err(
                    "Hidden or multiple-picture packets require full rendering for precise cuts."
                        .into(),
                );
            }
            if media.stream["codec_name"] != codec
                || s.width != info.width
                || s.height != info.height
                || s.bit_depth != info.bit_depth
                || s.pixel_format != info.pixel_format
                || s.colour_primaries != info.colour_primaries
                || s.colour_transfer != info.colour_transfer
                || s.colour_space != info.colour_space
                || s.colour_range != info.colour_range
                || s.hdr_metadata != info.hdr_metadata
            {
                return Ok(None);
            }
            if sources
                .values()
                .any(|m: &PacketMedia| m.configuration() != media.configuration())
            {
                return Err("Segments have incompatible video decoder configurations.".into());
            }
            sources.insert(clip.source_index, media);
        }
        let parent = output.parent().ok_or("Export needs a directory.")?;
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let scratch = parent.join(format!(
            ".freym-splice-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&scratch).map_err(|e| e.to_string())?;
        let _workspace = Workspace(scratch.clone());
        let mut paths = Vec::new();
        let mut durations = Vec::new();
        let mut expected = Vec::new();
        let mut copied = 0u64;
        let mut encoded = 0u64;
        let mut offset = 0u64;
        progress.store(0, Ordering::Relaxed);
        for clip in &timeline.clips {
            let path = &timeline.sources[clip.source_index].path;
            let media = &sources[&clip.source_index];
            let s = &info.segments[clip.source_index];
            let keys: Vec<u64> = media
                .packets
                .iter()
                .filter(|p| p.key && p.pts >= 0)
                .map(|p| p.pts as u64)
                .collect();
            let mut start = None;
            for &key in keys.iter().filter(|&&k| k >= clip.in_ns && k < clip.out_ns) {
                check_cancel(cancel)?;
                if self.independent(path, media, key)? {
                    start = Some(key);
                    break;
                }
            }
            let Some(mut start) = start else {
                return Err("Selected range has no independent region to stream-copy.".into());
            };
            // Preserve an original initial picture offset (and its audio offset).
            if clip.in_ns == 0 && media.packets.first().is_some_and(|p| p.pts == start as i64) {
                start = 0;
            }
            let video_end = s.frames.last().map(|(t, d)| t.0 + d).unwrap_or(0);
            let end_on_frame = s.frames.iter().any(|(t, _)| t.0 == clip.out_ns)
                || clip.out_ns == video_end
                || clip.out_ns == s.duration_ns;
            let closed_end = media.reordered() || codec == "av1";
            let mut end = if end_on_frame
                && (!closed_end || clip.out_ns == s.duration_ns || clip.out_ns == video_end)
            {
                Some(clip.out_ns)
            } else {
                None
            };
            if end.is_none() {
                for &key in keys
                    .iter()
                    .rev()
                    .filter(|&&k| k > start && k <= clip.out_ns)
                {
                    check_cancel(cancel)?;
                    if self.independent(path, media, key)? {
                        end = Some(key);
                        break;
                    }
                }
            }
            let Some(end) = end.filter(|&e| e > start) else {
                return Err("No complete compatible packet region survives this cut.".into());
            };
            for (a, b, copy) in [
                (clip.in_ns, start, false),
                (start, end, true),
                (end, clip.out_ns, false),
            ] {
                if a == b {
                    continue;
                }
                check_cancel(cancel)?;
                let part = scratch.join(format!("video-{}.mkv", paths.len()));
                let mut packets = if copy {
                    self.copy_video(media, a..b, &part, cancel)?
                } else {
                    self.encode_boundary(media, s, a..b, profile, &part, cancel)?
                };
                if copy {
                    copied += packets.len() as u64;
                } else {
                    encoded += packets.len() as u64;
                }
                for p in &mut packets {
                    p.pts += offset as i64;
                    p.dts = p.dts.map(|d| d + offset as i64);
                }
                expected.extend(packets);
                offset += b - a;
                paths.push(part);
                durations.push(b - a);
                progress.store(copied + encoded, Ordering::Relaxed);
            }
        }
        if copied == 0 {
            return Ok(None);
        }
        let list = scratch.join("video.txt");
        timed_concat(&paths, &durations, &list)?;
        let video = scratch.join("video.mkv");
        let mut cmd = command(&self.ffmpeg);
        cmd.arg("-copyts");
        cmd.args([
            "-v",
            "error",
            "-xerror",
            "-nostdin",
            "-n",
            "-f",
            "concat",
            "-safe",
            "0",
            "-auto_convert",
            "0",
            "-i",
        ])
        .arg(&list)
        .args([
            "-map",
            "0:v:0",
            "-c:v",
            "copy",
            "-avoid_negative_ts",
            "disabled",
            "-f",
            "matroska",
        ])
        .arg(&video);
        run(cmd, &scratch.join("video-join.log"), cancel)?;
        let (audio, audio_copied) = self.export_audio(timeline, info, &scratch, cancel)?;
        let final_path = scratch.join("export.mkv");
        let mut cmd = command(&self.ffmpeg);
        cmd.args(["-v", "error", "-xerror", "-nostdin", "-n", "-copyts", "-i"])
            .arg(&video);
        for path in &audio {
            cmd.arg("-i").arg(path);
        }
        cmd.args(["-map", "0:v:0"]);
        for i in 0..audio.len() {
            cmd.args(["-map", &format!("{}:a?", i + 1)]);
        }
        for (i, a) in info.audio.iter().enumerate() {
            cmd.args([&format!("-metadata:s:a:{i}"), &format!("title={}", a.title)]);
        }
        cmd.args([
            "-c",
            "copy",
            "-avoid_negative_ts",
            "disabled",
            "-f",
            "matroska",
        ])
        .arg(&final_path);
        run(cmd, &scratch.join("final-mux.log"), cancel)?;
        let final_packets = self.packet_media(&final_path, "v:0")?;
        check_packets(&final_packets.packets, &expected, 0)?;
        if final_packets.configuration() != sources.values().next().unwrap().configuration() {
            return Err("Joined output changed decoder configuration.".into());
        }
        for (track, input) in audio.iter().enumerate() {
            // Whole unchanged clips map every original audio stream from one input.
            let whole = audio.len() == 1
                && timeline.clips.len() == 1
                && timeline.clips[0].in_ns == 0
                && timeline.clips[0].out_ns
                    == info.segments[timeline.clips[0].source_index].duration_ns;
            let count = if whole { info.audio_tracks } else { 1 };
            for ordinal in 0..count {
                let from = self.packet_media(input, &format!("a:{ordinal}"))?;
                let to = self.packet_media(
                    &final_path,
                    &format!("a:{}", if whole { ordinal } else { track }),
                )?;
                if from.configuration() != to.configuration() {
                    return Err("Mux changed independent audio configuration.".into());
                }
                check_packets(&to.packets, &from.packets, 0)?;
            }
        }
        // One decode integrity check is necessary for a hybrid reference chain.
        let mut cmd = command(&self.ffmpeg);
        cmd.args(["-v", "error", "-xerror", "-nostdin", "-i"])
            .arg(&final_path)
            .args(["-map", "0:v:0", "-map", "0:a?", "-f", "null", "-"]);
        run(cmd, &scratch.join("verify-decode.log"), cancel)?;
        let exported = self.probe(&final_path)?;
        if exported.frames.len() != info.frames.len()
            || exported.audio != info.audio
            || exported.width != info.width
            || exported.height != info.height
            || exported.bit_depth != info.bit_depth
            || exported.pixel_format != info.pixel_format
            || exported.colour_primaries != info.colour_primaries
            || exported.colour_transfer != info.colour_transfer
            || exported.colour_space != info.colour_space
            || exported.colour_range != info.colour_range
            || exported.hdr_metadata != info.hdr_metadata
            || exported
                .frames
                .iter()
                .zip(&info.frames)
                .any(|((a, _), (b, _))| a.0.abs_diff(b.0) > PRECISION_NS as u64)
            || exported.duration_ns.abs_diff(info.duration_ns) > PRECISION_NS as u64
        {
            return Err("Packet export failed edit timing, track or colour verification.".into());
        }
        timeline.verify_source()?;
        check_cancel(cancel)?;
        OpenOptions::new()
            .write(true)
            .open(&final_path)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        fs::hard_link(&final_path, output)
            .map_err(|e| format!("Could not exclusively publish the export: {e}"))?;
        Ok(Some(ExportReport {
            frames: info.frames.len() as u64,
            audio_tracks: info.audio_tracks,
            duration_ns: info.duration_ns,
            method: if encoded == 0 && audio_copied {
                ExportMethod::StreamCopy
            } else {
                ExportMethod::SmartRender
            },
            copied_video_frames: copied,
            reencoded_video_frames: encoded,
            fallback_reason: None,
        }))
    }
    pub(crate) fn try_copy_audio_track(
        &self,
        timeline: &Timeline,
        info: &MediaInfo,
        audio: &AudioInfo,
        track: usize,
        scratch: &Path,
        cancel: &AtomicBool,
    ) -> StudioResult<Option<PathBuf>> {
        // Opus/AAC require decoder pre-roll and encoder delay at new cuts. Retain
        // the existing sample-accurate FLAC path rather than copying incomplete state.
        let attempt = || -> StudioResult<PathBuf> {
            let mut paths = Vec::new();
            let mut expected = Vec::new();
            let mut durations = Vec::new();
            let mut offset = 0i64;
            let mut config = None;
            for (index, clip) in timeline.clips.iter().enumerate() {
                check_cancel(cancel)?;
                let s = &info.segments[clip.source_index];
                let ordinal = s
                    .audio
                    .iter()
                    .position(|a| a.same_track(audio))
                    .ok_or("Audio track requires silence padding.")?;
                let select = format!("a:{ordinal}");
                let media =
                    self.packet_media(&timeline.sources[clip.source_index].path, &select)?;
                let codec = media.stream["codec_name"].as_str().unwrap_or("");
                if !(codec == "flac" || codec.starts_with("pcm_"))
                    || s.audio[ordinal].sample_rate != audio.sample_rate
                {
                    return Err("Audio cut requires sample-accurate decoding.".into());
                }
                if codec == "flac" {
                    let config = hex_dump(&media.stream["extradata"])?;
                    // A STREAMINFO digest/sample total for the original whole
                    // recording cannot describe a packet cut or a joined stream.
                    if config.len() != 34
                        || config[13] & 0x0f != 0
                        || config[14..18].iter().any(|&b| b != 0)
                        || config[18..34].iter().any(|&b| b != 0)
                    {
                        return Err(
                            "FLAC whole-stream integrity metadata requires audio rendering.".into(),
                        );
                    }
                }
                let c = media.configuration();
                if config.as_ref().is_some_and(|prior| prior != &c) {
                    return Err("Audio configurations differ.".into());
                }
                config = Some(c);
                let selected = media.range(clip.in_ns, clip.out_ns)?;
                if selected[0].pts != clip.in_ns as i64
                    || selected
                        .last()
                        .is_none_or(|p| p.pts + p.duration != clip.out_ns as i64)
                    || selected
                        .windows(2)
                        .any(|p| (p[0].pts + p[0].duration - p[1].pts).abs() > PRECISION_NS)
                    || selected.iter().any(|p| !p.side.is_null())
                {
                    return Err("Audio packets do not exactly cover the cut.".into());
                }
                let part = scratch.join(format!("copy-audio-{track}-{index}.mka"));
                let mut cmd = command(&self.ffmpeg);
                cmd.args(["-v", "error", "-xerror", "-nostdin", "-n", "-copyts", "-i"])
                    .arg(&timeline.sources[clip.source_index].path)
                    .args([
                        "-ss",
                        &seconds(clip.in_ns),
                        "-t",
                        &seconds(clip.duration_ns()),
                        "-map",
                        &format!("0:{select}"),
                        "-vn",
                        "-c:a",
                        "copy",
                        "-avoid_negative_ts",
                        "disabled",
                        "-f",
                        "matroska",
                    ])
                    .arg(&part);
                run(cmd, &scratch.join("audio-copy.log"), cancel)?;
                let actual = self.packet_media(&part, "a:0")?;
                check_packets(&actual.packets, &selected, -(clip.in_ns as i64))?;
                let mut packets = actual.packets;
                for p in &mut packets {
                    p.pts += offset;
                    p.dts = p.dts.map(|d| d + offset);
                }
                expected.extend(packets);
                offset += clip.duration_ns() as i64;
                paths.push(part);
                durations.push(clip.duration_ns());
            }
            let list = scratch.join(format!("copy-audio-{track}.txt"));
            timed_concat(&paths, &durations, &list)?;
            let joined = scratch.join(format!("copy-audio-{track}.mka"));
            let mut cmd = command(&self.ffmpeg);
            cmd.arg("-copyts");
            cmd.args([
                "-v", "error", "-xerror", "-nostdin", "-n", "-f", "concat", "-safe", "0", "-i",
            ])
            .arg(&list)
            .args([
                "-map",
                "0:a:0",
                "-c:a",
                "copy",
                "-metadata:s:a:0",
                &format!("title={}", audio.title),
                "-avoid_negative_ts",
                "disabled",
                "-f",
                "matroska",
            ])
            .arg(&joined);
            run(cmd, &scratch.join("audio-copy-join.log"), cancel)?;
            check_packets(&self.packet_media(&joined, "a:0")?.packets, &expected, 0)?;
            Ok(joined)
        };
        match attempt() {
            Ok(path) => Ok(Some(path)),
            Err(_) => {
                check_cancel(cancel)?;
                Ok(None)
            }
        }
    }
}
fn timed_concat(paths: &[PathBuf], durations: &[u64], list: &Path) -> StudioResult<()> {
    concat_file(paths, list)?;
    let text = fs::read_to_string(list).map_err(|e| e.to_string())?;
    let mut result = String::from("ffconcat version 1.0\n");
    for (line, duration) in text.lines().skip(1).zip(durations) {
        // Explicit origin retains a delayed first picture/audio offset rather
        // than letting concat subtract each file's detected start_time.
        result.push_str(&format!(
            "{line}\ninpoint 0\nduration {}\n",
            seconds(*duration)
        ));
    }
    fs::write(list, result).map_err(|e| e.to_string())
}
