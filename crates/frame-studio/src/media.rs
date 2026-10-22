use crate::assembly::preview_filter;
use crate::*;
use frame_clock::MediaTimestamp;
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
pub(crate) fn probe_output(mut cmd: Command) -> StudioResult<Vec<u8>> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            format!("Local Studio tool {program} is unavailable: {e}. Install matching FFmpeg/ffprobe or configure FREYM_FFMPEG and FREYM_FFPROBE.")
        })?;
    let mut output = Vec::new();
    let read = child
        .stdout
        .take()
        .ok_or("Studio tool output pipe unavailable.")?
        .take(MAX_DOCUMENT_BYTES + 1)
        .read_to_end(&mut output);
    if read.is_err() || output.len() as u64 > MAX_DOCUMENT_BYTES {
        let _ = child.kill();
        let _ = child.wait();
        return Err("Could not read a bounded source frame timeline.".into());
    }
    if !child.wait().map_err(|e| e.to_string())?.success() {
        return Err(format!(
            "Studio tool {program} could not inspect this media or capability. Check the local FFmpeg installation and media compatibility."
        ));
    }
    Ok(output)
}

#[derive(Clone, Debug)]
pub struct MediaInfo {
    pub video_codec: String,
    pub width: u32,
    pub height: u32,
    pub rate_num: u32,
    pub rate_den: u32,
    pub frames: Vec<(MediaTimestamp, u64)>,
    pub duration_ns: u64,
    pub audio_tracks: usize,
    pub colour_primaries: Option<String>,
    pub colour_transfer: Option<String>,
    pub colour_space: Option<String>,
    pub colour_range: Option<String>,
    pub pixel_format: String,
    pub bit_depth: u8,
    pub audio: Vec<AudioInfo>,
    pub hdr_metadata: Vec<serde_json::Value>,
    /// Source facts stay separate from assembled output timestamps.
    pub segments: Vec<MediaInfo>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioInfo {
    pub title: String,
    pub channels: u32,
    pub sample_rate: u32,
    pub ordinal: usize,
}
impl AudioInfo {
    /// Device/title and ordinal keep duplicate independent stems distinct.
    /// Sample-rate changes use explicit resampling, not a new logical track.
    pub fn same_track(&self, other: &Self) -> bool {
        self.title == other.title
            && self.ordinal == other.ordinal
            && self.channels == other.channels
    }
}
impl MediaInfo {
    /// Start Studio exports in the source codec so packet preservation is available.
    /// User-selected formats subsequently override this initial choice.
    pub fn default_export_profile(&self) -> ExportProfile {
        match self.video_codec.as_str() {
            "av1" => ExportProfile::Av1 { crf: 24 },
            "hevc" => ExportProfile::Hevc { crf: 18 },
            _ if self.bit_depth > 8 || self.hdr() => ExportProfile::Hevc { crf: 18 },
            _ => ExportProfile::default(),
        }
    }
    pub fn hdr(&self) -> bool {
        matches!(
            self.colour_transfer.as_deref(),
            Some("smpte2084" | "arib-std-b67")
        )
    }
}
#[derive(Clone, Debug)]
pub struct LocalTools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}
impl Default for LocalTools {
    fn default() -> Self {
        Self {
            ffmpeg: crate::tools::tool_path("ffmpeg"),
            ffprobe: crate::tools::tool_path("ffprobe"),
        }
    }
}
pub(crate) fn command(program: &Path) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    cmd.stdin(Stdio::null());
    cmd
}
fn seconds(value: &serde_json::Value) -> Option<u64> {
    let value = value.as_str()?.parse::<f64>().ok()?;
    if !value.is_finite() || value < 0.0 || value > u64::MAX as f64 / 1e9 {
        return None;
    }
    Some((value * 1e9).round() as u64)
}
impl LocalTools {
    pub fn probe(&self, source: &Path) -> StudioResult<MediaInfo> {
        let mut cmd = command(&self.ffprobe);
        cmd.args([
            "-v",
            "error",
            "-show_streams",
            "-show_format",
            "-of",
            "json",
        ])
        .arg(source);
        let result = probe_output(cmd)?;
        let data: serde_json::Value = serde_json::from_slice(&result).map_err(|e| e.to_string())?;
        let streams = data["streams"].as_array().ok_or("No media streams.")?;
        let container_duration_ns = seconds(&data["format"]["duration"]).unwrap_or(0);
        if streams
            .iter()
            .filter(|s| s["codec_type"] == "video")
            .count()
            != 1
        {
            return Err("Studio currently opens one video stream per clip.".into());
        }
        let video = streams
            .iter()
            .find(|s| s["codec_type"] == "video")
            .ok_or("No video stream.")?;
        let width = video["width"].as_u64().ok_or("Missing video width.")? as u32;
        let height = video["height"].as_u64().ok_or("Missing video height.")? as u32;
        if width == 0 || height == 0 || width as u64 * height as u64 > 33_554_432 {
            return Err("Studio supports up to 32 megapixels per frame.".into());
        }
        let pixel_format = video["pix_fmt"]
            .as_str()
            .ok_or("Missing pixel format.")?
            .to_owned();
        let bit_depth = if pixel_format.contains("10") { 10 } else { 8 };
        if pixel_format.contains("12")
            || pixel_format.contains("16")
            || pixel_format.contains("f32")
        {
            return Err("Studio currently supports 8-bit and 10-bit integer video. This source format is unsupported.".into());
        }
        if bit_depth == 10
            && !matches!(
                pixel_format.as_str(),
                "yuv420p10le" | "yuv422p10le" | "yuv444p10le" | "gbrp10le"
            )
        {
            return Err("Unsupported 10-bit input layout; use a planar 10-bit format.".into());
        }
        if video["sample_aspect_ratio"]
            .as_str()
            .is_some_and(|s| s != "1:1" && s != "N/A" && s != "0:1")
            || video["side_data_list"]
                .as_array()
                .is_some_and(|a| a.iter().any(|s| s.get("rotation").is_some()))
        {
            return Err("Rotated or non-square-pixel media is not yet supported.".into());
        }
        let rate = video["r_frame_rate"].as_str().unwrap_or("0/1");
        let (rate_num, rate_den) = rate
            .split_once('/')
            .and_then(|(a, b)| Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?)))
            .filter(|(a, b)| *a > 0 && *b > 0)
            .ok_or("Missing native video rate.")?;
        let mut cmd = command(&self.ffprobe);
        cmd.args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_frames",
            "-show_entries",
            "frame=best_effort_timestamp_time,duration_time,pkt_duration_time,side_data_list",
            "-of",
            "json",
        ])
        .arg(source);
        let result = probe_output(cmd)?;
        let data: serde_json::Value = serde_json::from_slice(&result).map_err(|e| e.to_string())?;
        let rows = data["frames"]
            .as_array()
            .ok_or("No decoded video timeline.")?;
        if rows.is_empty() || rows.len() > 2_000_000 {
            return Err(
                "Source frame timeline is empty or too large for this Studio version.".into(),
            );
        }
        let mut frames: Vec<(MediaTimestamp, u64)> = Vec::with_capacity(rows.len());
        for row in rows {
            let pts = seconds(&row["best_effort_timestamp_time"])
                .ok_or("Source contains missing or negative video timestamps.")?;
            if frames.last().is_some_and(|(last, _)| last.0 >= pts) {
                return Err("Source video timestamps are not strictly increasing.".into());
            }
            frames.push((
                MediaTimestamp(pts),
                seconds(&row["duration_time"])
                    .or_else(|| seconds(&row["pkt_duration_time"]))
                    .unwrap_or(0),
            ));
        }
        for i in 0..frames.len() - 1 {
            frames[i].1 = frame_encode::duration_until(frames[i].0, frames[i + 1].0)
                .map_err(|e| e.to_string())?
                .get();
        }
        let last = frames.last_mut().unwrap();
        if last.1 == 0 {
            last.1 = frame_encode::nominal_duration(
                std::num::NonZeroU32::new(rate_num).unwrap(),
                std::num::NonZeroU32::new(rate_den).unwrap(),
            )
            .get();
        }
        let duration_ns = last
            .0
            .0
            .checked_add(last.1)
            .ok_or("Source duration overflow.")?;
        let duration_ns = duration_ns.max(container_duration_ns);
        Ok(MediaInfo {
            video_codec: video["codec_name"].as_str().unwrap_or_default().to_owned(),
            width,
            height,
            rate_num,
            rate_den,
            frames,
            duration_ns,
            audio_tracks: streams
                .iter()
                .filter(|s| s["codec_type"] == "audio")
                .count(),
            colour_primaries: video["color_primaries"].as_str().map(str::to_owned),
            colour_transfer: video["color_transfer"].as_str().map(str::to_owned),
            colour_space: video["color_space"].as_str().map(str::to_owned),
            colour_range: video["color_range"].as_str().map(str::to_owned),
            pixel_format,
            bit_depth,
            audio: streams
                .iter()
                .filter(|s| s["codec_type"] == "audio")
                .enumerate()
                .map(|(i, s)| AudioInfo {
                    ordinal: i,
                    title: s["tags"]["title"]
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("Audio {}", i + 1))
                        .chars()
                        .take(240)
                        .collect::<String>(),
                    channels: s["channels"].as_u64().unwrap_or(2) as u32,
                    sample_rate: s["sample_rate"]
                        .as_str()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(48000),
                })
                .enumerate()
                .map(|(i, mut a)| {
                    if a.title.is_empty() {
                        a.title = format!("Audio {}", i + 1);
                    }
                    a
                })
                .collect(),
            hdr_metadata: {
                let mut metadata = rows[0]["side_data_list"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                for item in video["side_data_list"].as_array().into_iter().flatten() {
                    if !metadata
                        .iter()
                        .any(|m| m["side_data_type"] == item["side_data_type"])
                    {
                        metadata.push(item.clone());
                    }
                }
                metadata.retain(|s| {
                    matches!(
                        s["side_data_type"].as_str(),
                        Some("Mastering display metadata" | "Content light level metadata")
                    )
                });
                // Container floats and codec rationals use different denominators.
                // Normalize to the native mastering metadata units before comparison.
                for item in &mut metadata {
                    if item["side_data_type"] == "Mastering display metadata" {
                        for key in [
                            "red_x",
                            "red_y",
                            "green_x",
                            "green_y",
                            "blue_x",
                            "blue_y",
                            "white_point_x",
                            "white_point_y",
                            "min_luminance",
                            "max_luminance",
                        ] {
                            let units = if key.ends_with("luminance") {
                                10000.
                            } else {
                                50000.
                            };
                            if let Some((a, b)) = item[key].as_str().and_then(|v| v.split_once('/'))
                                && let (Ok(a), Ok(b)) = (a.parse::<f64>(), b.parse::<f64>())
                                && a.is_finite()
                                && b.is_finite()
                                && b > 0.
                                && a >= 0.
                            {
                                item[key] = serde_json::Value::String(format!(
                                    "{}/{}",
                                    (a / b * units).round() as u64,
                                    units as u64
                                ));
                            }
                        }
                    }
                }
                metadata.sort_by(|a, b| {
                    a["side_data_type"]
                        .as_str()
                        .cmp(&b["side_data_type"].as_str())
                });
                metadata
            },
            segments: Vec::new(),
        })
    }
    pub fn open(&self, input: &Path) -> StudioResult<(Timeline, MediaInfo)> {
        self.capabilities()?;
        let input = std::fs::canonicalize(input).map_err(|e| e.to_string())?;
        if input.to_string_lossy().ends_with(".freym-edit.json") {
            let timeline = Timeline::load(&input)?;
            let info = self.inspect_timeline(&timeline)?;
            return Ok((timeline, info));
        }
        let manifest_path = self.find_manifest(&input)?;
        let anchor = manifest_path.as_ref().unwrap_or(&input);
        let edit_path = sidecar_path(anchor, ".freym-edit.json");
        // Old single-file edits remain loadable; do not silently replace their decisions.
        let legacy_edit = sidecar_path(&input, ".freym-edit.json");
        let edit = if edit_path.exists() {
            Some(edit_path)
        } else if legacy_edit.exists() {
            Some(legacy_edit)
        } else {
            None
        };
        if let Some(edit) = edit {
            let timeline = Timeline::load(&edit)?;
            if timeline.source != *anchor && timeline.source != input {
                return Err("Saved edit refers to a different source.".into());
            }
            return Ok((timeline.clone(), self.inspect_timeline(&timeline)?));
        }
        let identity = SourceIdentity::read(anchor)?;
        let mut timeline = Timeline {
            version: 2,
            source: anchor.clone(),
            identity,
            duration_ns: 0,
            metadata: None,
            zooms: Vec::new(),
            cursor: CursorEffects::default(),
            sources: Vec::new(),
            clips: Vec::new(),
            warnings: Vec::new(),
            output_colour: OutputColour::default(),
        };
        let mut candidates = Vec::new();
        let mut metadata_path = None;
        if let Some(path) = &manifest_path {
            let mut manifest = frame_mux::read_session_manifest(path).map_err(|e| e.to_string())?;
            let mut folder = path.parent().unwrap().to_path_buf();
            if !manifest.completed {
                timeline.warnings.push("Incomplete session: only verified segments and recovered playable prefixes are included.".into());
                if manifest.segments.iter().any(|s| {
                    matches!(
                        s.state,
                        frame_mux::SegmentState::Open | frame_mux::SegmentState::Interrupted
                    )
                }) {
                    let recovery_dir = folder.join(format!(
                        "studio-recovery-{}-{}",
                        std::process::id(),
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos()
                    ));
                    std::fs::create_dir(&recovery_dir).map_err(|e| e.to_string())?;
                    match frame_mux::recover_session(
                        path,
                        &recovery_dir.join("recording.freym-session"),
                    ) {
                        Ok(recovery) => {
                            for (name, why) in &recovery.rejected_segments {
                                timeline.warnings.push(format!("Skipped {name}: {why}"));
                            }
                            if let Some(error) = recovery.metadata_error {
                                timeline.warnings.push(error);
                            }
                            manifest = recovery.manifest;
                            folder = recovery_dir;
                            timeline.warnings.push(format!(
                                "Recovered copies kept in {}. Originals are preserved.",
                                folder.display()
                            ));
                        }
                        Err(error) => timeline.warnings.push(format!(
                            "Tail recovery unavailable: {}. Closed segments remain editable.",
                            error.message
                        )),
                    }
                }
            }
            metadata_path = manifest
                .metadata_file
                .as_ref()
                .map(|name| folder.join(name));
            for segment in &manifest.segments {
                let media = folder.join(&segment.file_name);
                match frame_mux::verify_session_segment(&media, segment) {
                    Ok(()) => candidates.push((media, segment.origin_ns)),
                    Err(e) => timeline
                        .warnings
                        .push(format!("Skipped {}: {}", segment.file_name, e.message)),
                }
            }
        } else {
            candidates.push((input.clone(), 0));
        }
        for (path, origin_ns) in candidates {
            let identity = match SourceIdentity::read(&path) {
                Ok(v) => v,
                Err(e) => {
                    timeline.warnings.push(e);
                    continue;
                }
            };
            let info = match self.probe(&path) {
                Ok(v) => v,
                Err(e) => {
                    timeline
                        .warnings
                        .push(format!("Skipped {}: {e}", path.display()));
                    continue;
                }
            };
            let metadata = match &metadata_path {
                Some(path) => {
                    match read_recording_metadata(path, origin_ns, info.width, info.height, true) {
                        Ok(m) => Some(m),
                        Err(e) => {
                            timeline
                                .warnings
                                .push(format!("Interaction metadata unavailable: {e}"));
                            None
                        }
                    }
                }
                None => None,
            };
            timeline.sources.push(SourceSegment {
                path,
                identity,
                origin_ns,
                duration_ns: info.duration_ns,
                metadata,
            });
        }
        if timeline.sources.is_empty() {
            return Err(format!(
                "No editable media remains. {}",
                timeline.warnings.join(" ")
            ));
        }
        // Session timestamps already exclude recorder pauses. Keep encoder restart gaps
        // by holding the preceding picture and padding each independent audio track.
        for i in 0..timeline.sources.len().saturating_sub(1) {
            let span = timeline.sources[i + 1]
                .origin_ns
                .saturating_sub(timeline.sources[i].origin_ns);
            if span > 0 {
                timeline.sources[i].duration_ns = span;
            }
        }
        timeline.clips = timeline
            .sources
            .iter()
            .enumerate()
            .map(|(source_index, s)| TimelineClip {
                source_index,
                in_ns: 0,
                out_ns: s.duration_ns,
            })
            .collect();
        timeline.duration_ns = timeline.clips.iter().map(TimelineClip::duration_ns).sum();
        timeline.rebuild_metadata();
        timeline.verify_source()?;
        let info = self.inspect_timeline(&timeline)?;
        Ok((timeline, info))
    }
    fn find_manifest(&self, input: &Path) -> StudioResult<Option<PathBuf>> {
        if input.to_string_lossy().ends_with(".freym-session") {
            return Ok(Some(input.to_path_buf()));
        }
        let direct = sidecar_path(input, ".freym-session");
        if direct.is_file() {
            return Ok(Some(direct));
        }
        let Some(folder) = input.parent() else {
            return Ok(None);
        };
        // Opening a later segment resolves its owning session, too. Bounded scan.
        for entry in std::fs::read_dir(folder)
            .map_err(|e| e.to_string())?
            .take(4096)
        {
            let path = entry.map_err(|e| e.to_string())?.path();
            if !path.to_string_lossy().ends_with(".freym-session") {
                continue;
            }
            if let Ok(manifest) = frame_mux::read_session_manifest(&path)
                && manifest
                    .segments
                    .iter()
                    .any(|s| input.file_name().is_some_and(|n| n == s.file_name.as_str()))
            {
                return Ok(Some(path));
            }
        }
        Ok(None)
    }
    pub fn inspect_timeline(&self, timeline: &Timeline) -> StudioResult<MediaInfo> {
        timeline.verify_source()?;
        if timeline.version == 1 {
            let mut info = self.probe(&timeline.source)?;
            let video_end = info
                .frames
                .last()
                .map_or(0, |(pts, duration)| pts.0 + duration);
            // Original v1 projects measured video duration only. New probing
            // includes audio tails; retain the saved project's exact edit clock.
            if (timeline.duration_ns != info.duration_ns && timeline.duration_ns != video_end)
                || timeline
                    .metadata
                    .as_ref()
                    .is_some_and(|m| m.width != info.width || m.height != info.height)
            {
                return Err("Saved edit geometry or duration does not match the source.".into());
            }
            info.duration_ns = timeline.duration_ns;
            return Ok(info);
        }
        let segments: Vec<MediaInfo> = timeline
            .sources
            .iter()
            .map(|s| self.probe(&s.path))
            .collect::<StudioResult<_>>()?;
        self.assemble_timeline(timeline, segments)
    }
    /// Remap an edit of the same sources without decoding every source again.
    /// Identity verification still detects a changed or missing original.
    pub fn retime_timeline(
        &self,
        timeline: &Timeline,
        before: &Timeline,
        previous: &MediaInfo,
    ) -> StudioResult<MediaInfo> {
        timeline.verify_source()?;
        let same_sources = if before.version == 1 {
            timeline.sources.len() == 1
                && timeline.sources[0].path == before.source
                && timeline.sources[0].identity == before.identity
        } else {
            timeline.sources.len() == before.sources.len()
                && timeline
                    .sources
                    .iter()
                    .zip(&before.sources)
                    .all(|(a, b)| a.path == b.path && a.identity == b.identity)
        };
        if !same_sources || timeline.version != 2 {
            return self.inspect_timeline(timeline);
        }
        let segments = if previous.segments.is_empty() {
            vec![previous.clone()]
        } else {
            previous.segments.clone()
        };
        if segments.len() != timeline.sources.len() {
            return self.inspect_timeline(timeline);
        }
        self.assemble_timeline(timeline, segments)
    }
    fn assemble_timeline(
        &self,
        timeline: &Timeline,
        segments: Vec<MediaInfo>,
    ) -> StudioResult<MediaInfo> {
        let mut info = segments[timeline.clips[0].source_index].clone();
        info.frames.clear();
        let mut offset = 0;
        for clip in &timeline.clips {
            let source = &segments[clip.source_index];
            // Exact frame boundaries, including a retained picture at a non-frame trim.
            let first = source
                .frames
                .partition_point(|(t, _)| t.0 <= clip.in_ns)
                .saturating_sub(1);
            if source.frames[first].0.0 >= clip.out_ns {
                return Err("The selected clip ends before its first video picture. Keep a range containing playable video.".into());
            }
            for (frame_index, (pts, _)) in source.frames.iter().enumerate().skip(first) {
                if pts.0 >= clip.out_ns {
                    break;
                }
                let at = pts.0.max(clip.in_ns);
                let end = source
                    .frames
                    .get(frame_index + 1)
                    .map_or(clip.out_ns, |(next, _)| next.0.min(clip.out_ns));
                if end > at {
                    info.frames
                        .push((MediaTimestamp(offset + at - clip.in_ns), end - at));
                }
            }
            if let Some(last) = info.frames.last_mut() {
                last.1 = offset + clip.duration_ns() - last.0.0;
            }
            offset += clip.duration_ns();
            for audio in &source.audio {
                if !info.audio.iter().any(|a| a.same_track(audio)) {
                    info.audio.push(audio.clone());
                }
            }
            info.bit_depth = info.bit_depth.max(source.bit_depth);
        }
        if info.frames.is_empty() {
            return Err("Trim has no playable picture.".into());
        }
        info.audio_tracks = info.audio.len();
        info.duration_ns = timeline.duration_ns;
        info.segments = segments;
        Ok(info)
    }
    pub fn decode_frame(
        &self,
        timeline: &Timeline,
        info: &MediaInfo,
        time_ns: u64,
    ) -> StudioResult<Vec<u8>> {
        timeline.verify_source()?;
        let (source_path, source_info, local_time) = if timeline.version == 2 {
            let (clip_index, local, _) = timeline
                .locate(time_ns.min(timeline.duration_ns - 1))
                .ok_or("Seek outside timeline.")?;
            let clip = &timeline.clips[clip_index];
            (
                &timeline.sources[clip.source_index].path,
                &info.segments[clip.source_index],
                local,
            )
        } else {
            (&timeline.source, info, time_ns)
        };
        let index = source_info
            .frames
            .partition_point(|(t, _)| t.0 <= local_time)
            .saturating_sub(1);
        let timestamp = MediaTimestamp(time_ns);
        let preview_filter = preview_filter(source_info, info.width, info.height)?;
        let result = command(&self.ffmpeg)
            .args([
                "-v",
                "error",
                "-xerror",
                "-noautorotate",
                "-copyts",
                "-ss",
                &format!("{:.9}", source_info.frames[index].0.0 as f64 / 1e9),
                "-i",
            ])
            .arg(source_path)
            .args([
                "-map",
                "0:v:0",
                "-vf",
                &format!(
                    "select=gte(t\\,{:.9}),{preview_filter}",
                    source_info.frames[index].0.0 as f64 / 1e9
                ),
                "-frames:v",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgb24",
                "pipe:1",
            ])
            .output()
            .map_err(|e| e.to_string())?;
        if !result.status.success()
            || result.stdout.len() != info.width as usize * info.height as usize * 3
        {
            return Err(format!(
                "Frame decode failed: {}",
                String::from_utf8_lossy(&result.stderr)
            ));
        }
        Ok(render_rgb(
            timeline,
            timestamp,
            info.width,
            info.height,
            &result.stdout,
        ))
    }
}
