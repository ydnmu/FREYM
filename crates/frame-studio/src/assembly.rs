//! Reuses the raster mux and exclusive publication path for ordered source ranges.
use crate::render::{Process, Workspace, check_cancel};
use crate::*;
use frame_clock::MediaTimestamp;
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Duration,
};

pub(crate) fn preview_filter(info: &MediaInfo, width: u32, height: u32) -> StudioResult<String> {
    let scale = format!("scale={width}:{height}");
    if info.hdr() {
        if info.colour_primaries.as_deref() != Some("bt2020")
            || !(matches!(info.colour_space.as_deref(), Some("bt2020nc" | "gbr"))
                || (info.colour_space.is_none() && info.pixel_format.starts_with("gbr")))
        {
            return Err(
                "HDR preview requires declared BT.2020 non-constant luminance YUV or RGB colour."
                    .into(),
            );
        }
        let matrix = info.colour_space.as_deref().unwrap_or("gbr");
        let range =
            info.colour_range
                .as_deref()
                .unwrap_or(if matrix == "gbr" { "pc" } else { "tv" });
        let transfer = info
            .colour_transfer
            .as_deref()
            .ok_or("Missing HDR transfer.")?;
        // Explicit presentation-only conversion, never used in preserving export.
        Ok(format!(
            "zscale=pin=bt2020:tin={transfer}:min={matrix}:rin={range}:t=linear:npl=100,format=gbrpf32le,tonemap=tonemap=mobius,zscale=p=bt709:t=bt709:m=bt709:r=full,{scale},format=rgb24"
        ))
    } else {
        Ok(format!("{scale},format=rgb24"))
    }
}
fn seconds(ns: u64) -> String {
    format!("{:.9}", ns as f64 / 1e9)
}
pub(crate) fn run(mut cmd: Command, log: &Path, cancel: &AtomicBool) -> StudioResult<()> {
    let mut process = Process(
        cmd.stdout(Stdio::null())
            .stderr(File::create(log).map_err(|e| e.to_string())?)
            .spawn()
            .map_err(|e| e.to_string())?,
    );
    loop {
        check_cancel(cancel)?;
        if let Some(status) = process.0.try_wait().map_err(|e| e.to_string())? {
            if !status.success() {
                let mut detail = String::new();
                File::open(log)
                    .map_err(|e| e.to_string())?
                    .take(16 * 1024)
                    .read_to_string(&mut detail)
                    .map_err(|e| e.to_string())?;
                return Err(format!("Studio media processing failed: {detail}"));
            }
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
pub(crate) fn audio_filter(clip: &TimelineClip, rate: u32) -> String {
    format!(
        "atrim=start={}:end={},asetpts=PTS-{}/TB,aresample={rate}:async=1:first_pts=0,apad=whole_dur={},atrim=duration={}",
        seconds(clip.in_ns),
        seconds(clip.out_ns),
        seconds(clip.in_ns),
        seconds(clip.duration_ns()),
        seconds(clip.duration_ns())
    )
}
pub(crate) fn concat_file(paths: &[PathBuf], list: &Path) -> StudioResult<()> {
    let mut text = String::from("ffconcat version 1.0\n");
    for path in paths {
        let path = path
            .to_string_lossy()
            .replace('\\', "/")
            .replace('\'', "'\\''");
        if path.contains(['\n', '\r']) {
            return Err("Newlines in media paths are unsupported.".into());
        }
        text.push_str(&format!("file '{path}'\n"));
    }
    fs::write(list, text).map_err(|e| e.to_string())
}
impl LocalTools {
    pub(crate) fn export_assembled(
        &self,
        original: &Timeline,
        output: &Path,
        profile: ExportProfile,
        cancel: &AtomicBool,
        progress: &AtomicU64,
    ) -> StudioResult<ExportReport> {
        check_cancel(cancel)?;
        original.verify_source()?;
        if output.exists() {
            return Err("Export destination already exists. Choose a new filename.".into());
        }
        let mut timeline = original.clone();
        timeline.upgrade();
        // A split without a gap is still the same source interval. Coalesce only
        // this export snapshot; the user's ordered non-destructive edit stays intact.
        let mut clips: Vec<TimelineClip> = Vec::new();
        for clip in &timeline.clips {
            if let Some(last) = clips.last_mut()
                && last.source_index == clip.source_index
                && last.out_ns == clip.in_ns
            {
                last.out_ns = clip.out_ns;
            } else {
                clips.push(clip.clone());
            }
        }
        timeline.clips = clips;
        match profile {
            ExportProfile::H264 { crf } | ExportProfile::Hevc { crf } if crf > 51 => {
                return Err("CRF must be 0–51.".into());
            }
            ExportProfile::Av1 { crf } if crf > 63 => return Err("AV1 CRF must be 0–63.".into()),
            _ => (),
        }
        let mut info = self.inspect_timeline(&timeline)?;
        let fallback_reason =
            match self.try_packet_export(&timeline, &info, output, profile, cancel, progress) {
                Ok(Some(report)) => return Ok(report),
                Ok(None) => None,
                Err(reason) => {
                    check_cancel(cancel)?;
                    original.verify_source()?;
                    Some(reason)
                }
            };
        crate::colour::export_colour(&timeline, &mut info)?;
        if profile == ExportProfile::Lossless {
            let layout = info.segments[timeline.clips[0].source_index]
                .pixel_format
                .as_str();
            if layout.starts_with("yuv444p") || layout.starts_with("yuv422p") {
                info.pixel_format = format!(
                    "{}{}",
                    if layout.starts_with("yuv444p") {
                        "yuv444p"
                    } else {
                        "yuv422p"
                    },
                    if info.bit_depth == 10 { "10le" } else { "" }
                );
            }
        }
        self.require_encoder(profile, info.bit_depth)?;
        if profile != ExportProfile::Lossless && (info.width % 2 != 0 || info.height % 2 != 0) {
            return Err("4:2:0 export requires even dimensions. Select FFV1.".into());
        }
        if matches!(profile, ExportProfile::H264 { .. }) && (info.bit_depth > 8 || info.hdr()) {
            return Err("H.264 Studio export is SDR 8-bit. Select HEVC, AV1 or FFV1 to preserve this source.".into());
        }
        let colour_conversion = timeline.clips.iter().any(|c| {
            let s = &info.segments[c.source_index];
            s.colour_primaries != info.colour_primaries
                || s.colour_transfer != info.colour_transfer
                || s.colour_space != info.colour_space
                || s.colour_range != info.colour_range
        });
        let preserving = colour_conversion
            || info.bit_depth > 8
            || info.hdr()
            || info.colour_primaries.as_deref() == Some("bt2020");
        if preserving {
            info.colour_primaries.get_or_insert("bt709".into());
            info.colour_transfer.get_or_insert("bt709".into());
            info.colour_space.get_or_insert("bt709".into());
            info.colour_range.get_or_insert("tv".into());
        }
        let rgb16 = preserving && timeline.cursor.enabled;
        let needs_colour = rgb16
            || timeline.clips.iter().any(|c| {
                let s = &info.segments[c.source_index];
                s.hdr()
                    || s.colour_primaries != info.colour_primaries
                    || s.colour_transfer != info.colour_transfer
                    || s.colour_space != info.colour_space
                    || s.colour_range != info.colour_range
                    || s.bit_depth != info.bit_depth
            });
        if needs_colour && !self.capabilities()?.filters.contains("zscale") {
            return Err("This edit requires FFmpeg zscale for high precision colour conversion. No SDR fallback is used.".into());
        }
        if timeline
            .clips
            .iter()
            .any(|c| info.segments[c.source_index].hdr())
            && !info.hdr()
            && !self.capabilities()?.filters.contains("tonemap")
        {
            return Err(
                "Explicit HDR-to-SDR export requires FFmpeg tonemap. No output was published."
                    .into(),
            );
        }
        let parent = output.parent().ok_or("Export needs a directory.")?;
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let scratch = parent.join(format!(
            ".freym-render-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&scratch).map_err(|e| e.to_string())?;
        let _workspace = Workspace(scratch.clone());
        progress.store(0, Ordering::Relaxed);
        let rendered = scratch.join("rendered.mkv");
        if rgb16 {
            self.render_assembled_rgb16(&timeline, &info, &rendered, &scratch, cancel, progress)?;
        } else if preserving {
            self.render_preserving(&timeline, &info, &rendered, &scratch, cancel, progress)?;
        } else {
            self.render_assembled_rgb(&timeline, &info, &rendered, &scratch, cancel, progress)?;
        }
        let (audio_inputs, _) = self.export_audio(&timeline, &info, &scratch, cancel)?;
        let final_path = scratch.join("export.mkv");
        let mut cmd = command(&self.ffmpeg);
        cmd.args(["-v", "error", "-nostdin", "-n", "-copyts", "-i"])
            .arg(&rendered);
        for path in &audio_inputs {
            cmd.arg("-i").arg(path);
        }
        cmd.args(["-map", "0:v:0"]);
        for i in 0..audio_inputs.len() {
            cmd.args(["-map", &format!("{}:a?", i + 1)]);
        }
        cmd.args([
            "-c:a",
            "copy",
            "-fps_mode:v",
            "passthrough",
            "-enc_time_base:v",
            "1:1000000",
            "-avoid_negative_ts",
            "disabled",
        ]);
        self.configure_video_encoder(&mut cmd, profile, &info, preserving)?;
        if rgb16 {
            cmd.args(["-vf", &crate::colour::rgb_output_filter(&info)?]);
        }
        for (i, a) in info.audio.iter().enumerate() {
            cmd.args([&format!("-metadata:s:a:{i}"), &format!("title={}", a.title)]);
        }
        cmd.args(["-f", "matroska"]).arg(&final_path);
        run(cmd, &scratch.join("encode.log"), cancel)?;
        original.verify_source()?;
        check_cancel(cancel)?;
        // Verify before publication: no silent profile, bit-depth or colour fallback.
        let exported = self.probe(&final_path)?;
        if exported.frames.len() != info.frames.len()
            || exported.audio_tracks != info.audio_tracks
            || exported.bit_depth != info.bit_depth
            || exported.width != info.width
            || exported.height != info.height
            || (preserving
                && (exported.colour_primaries != info.colour_primaries
                    || exported.pixel_format != info.pixel_format
                    || exported.colour_transfer != info.colour_transfer
                    || exported.colour_space != info.colour_space
                    || exported.colour_range != info.colour_range))
        {
            return Err("Export verification rejected a frame/track count or preserving colour mismatch. No output was published.".into());
        }
        for ((a, _), (b, _)) in info.frames.iter().zip(&exported.frames) {
            if a.0.abs_diff(b.0) > 1_000_000 {
                return Err(
                    "Export timestamp verification failed. No output was published.".into(),
                );
            }
        }
        if preserving && info.hdr_metadata != exported.hdr_metadata {
            return Err("Export did not preserve supplied mastering/light metadata. No output was published.".into());
        }
        OpenOptions::new()
            .write(true)
            .open(&final_path)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        fs::hard_link(&final_path, output)
            .map_err(|e| format!("Could not exclusively publish the export: {e}"))?;
        Ok(ExportReport {
            frames: info.frames.len() as u64,
            audio_tracks: info.audio_tracks,
            duration_ns: info.duration_ns,
            method: ExportMethod::FullRender,
            copied_video_frames: 0,
            reencoded_video_frames: info.frames.len() as u64,
            fallback_reason,
        })
    }
    /// Shared independent-track editing for packet exports and rendered exports.
    pub(crate) fn export_audio(
        &self,
        timeline: &Timeline,
        info: &MediaInfo,
        scratch: &Path,
        cancel: &AtomicBool,
    ) -> StudioResult<(Vec<PathBuf>, bool)> {
        let single_copy = timeline.clips.len() == 1
            && timeline.clips[0].in_ns == 0
            && timeline.clips[0].out_ns
                == info.segments[timeline.clips[0].source_index].duration_ns;
        let mut audio_inputs = Vec::new();
        let mut all_copied = true;
        if single_copy {
            audio_inputs.push(
                timeline.sources[timeline.clips[0].source_index]
                    .path
                    .clone(),
            );
        } else {
            for (track, audio) in info.audio.iter().enumerate() {
                if let Some(path) =
                    self.try_copy_audio_track(timeline, info, audio, track, scratch, cancel)?
                {
                    audio_inputs.push(path);
                    continue;
                }
                all_copied = false;
                if !self.capabilities()?.encoders.contains("flac") {
                    return Err(
                        "FFmpeg FLAC support is required for sample-accurate audio edits.".into(),
                    );
                }
                let mut parts = Vec::new();
                for (index, clip) in timeline.clips.iter().enumerate() {
                    let path = scratch.join(format!("audio-{track}-{index}.mka"));
                    let s = &info.segments[clip.source_index];
                    let mut cmd = command(&self.ffmpeg);
                    cmd.args(["-v", "error", "-xerror", "-nostdin", "-n"]);
                    if let Some(a) = s.audio.iter().position(|a| a.same_track(audio)) {
                        cmd.args(["-copyts", "-i"])
                            .arg(&timeline.sources[clip.source_index].path)
                            .args([
                                "-map",
                                &format!("0:a:{a}"),
                                "-af",
                                &audio_filter(clip, audio.sample_rate),
                            ]);
                    } else {
                        cmd.args([
                            "-f",
                            "lavfi",
                            "-i",
                            &format!(
                                "anullsrc=r={}:cl={}",
                                audio.sample_rate,
                                if audio.channels == 1 {
                                    "mono"
                                } else {
                                    "stereo"
                                }
                            ),
                            "-t",
                            &seconds(clip.duration_ns()),
                        ]);
                        if audio.channels > 2 {
                            return Err("A missing multichannel track cannot yet be padded. Original independent tracks remain preserved.".into());
                        }
                    }
                    cmd.args([
                        "-vn",
                        "-c:a",
                        "flac",
                        "-metadata:s:a:0",
                        &format!("title={}", audio.title),
                    ])
                    .arg(&path);
                    run(cmd, &scratch.join("audio.log"), cancel)?;
                    parts.push(path);
                }
                let list = scratch.join(format!("audio-{track}.txt"));
                concat_file(&parts, &list)?;
                let joined = scratch.join(format!("audio-{track}.mka"));
                let mut cmd = command(&self.ffmpeg);
                cmd.args([
                    "-v", "error", "-nostdin", "-n", "-f", "concat", "-safe", "0", "-i",
                ])
                .arg(&list)
                .args([
                    "-map",
                    "0:a",
                    "-c:a",
                    "copy",
                    "-metadata:s:a:0",
                    &format!("title={}", audio.title),
                ])
                .arg(&joined);
                run(cmd, &scratch.join("audio-join.log"), cancel)?;
                audio_inputs.push(joined);
            }
        }
        Ok((audio_inputs, all_copied))
    }
    fn render_assembled_rgb(
        &self,
        timeline: &Timeline,
        info: &MediaInfo,
        output: &Path,
        scratch: &Path,
        cancel: &AtomicBool,
        progress: &AtomicU64,
    ) -> StudioResult<()> {
        let mut writer = frame_mux::MkvRasterWriter::create(output, info.width, info.height)?;
        let mut offset = 0;
        let mut frame_count = 0;
        for clip in &timeline.clips {
            let source = &info.segments[clip.source_index];
            let first = source
                .frames
                .partition_point(|(t, _)| t.0 <= clip.in_ns)
                .saturating_sub(1);
            let end = source.frames.partition_point(|(t, _)| t.0 < clip.out_ns);
            if end <= first {
                return Err("Selected clip has no decoded frames.".into());
            }
            let mut decoder = Process(
                command(&self.ffmpeg)
                    .args(["-v", "error", "-xerror", "-nostdin", "-noautorotate", "-i"])
                    .arg(&timeline.sources[clip.source_index].path)
                    .args([
                        "-map",
                        "0:v:0",
                        "-an",
                        "-vf",
                        &format!(
                            "select=between(n\\,{first}\\,{}),{}",
                            end - 1,
                            preview_filter(source, info.width, info.height)?
                        ),
                        "-fps_mode",
                        "passthrough",
                        "-f",
                        "rawvideo",
                        "pipe:1",
                    ])
                    .stdout(Stdio::piped())
                    .stderr(File::create(scratch.join("decode.log")).map_err(|e| e.to_string())?)
                    .spawn()
                    .map_err(|e| e.to_string())?,
            );
            let mut stdout = decoder.0.stdout.take().ok_or("Decoder pipe unavailable.")?;
            let mut pixels = vec![0u8; info.width as usize * info.height as usize * 3];
            for index in first..end {
                check_cancel(cancel)?;
                stdout
                    .read_exact(&mut pixels)
                    .map_err(|e| format!("Source decode stopped: {e}"))?;
                let at = offset + source.frames[index].0.0.max(clip.in_ns) - clip.in_ns;
                let end = if index + 1 < end {
                    offset + source.frames[index + 1].0.0 - clip.in_ns
                } else {
                    offset + clip.duration_ns()
                };
                let rgb = render_rgb(
                    timeline,
                    MediaTimestamp(at),
                    info.width,
                    info.height,
                    &pixels,
                );
                writer.write(
                    MediaTimestamp(at),
                    end - at,
                    &encode_png(info.width, info.height, &rgb)?,
                )?;
                frame_count += 1;
                progress.store(frame_count, Ordering::Relaxed);
            }
            if !decoder.0.wait().map_err(|e| e.to_string())?.success() {
                return Err("Source decode failed. No output published.".into());
            }
            offset += clip.duration_ns();
        }
        writer.finish()
    }
    fn render_assembled_rgb16(
        &self,
        timeline: &Timeline,
        info: &MediaInfo,
        output: &Path,
        scratch: &Path,
        cancel: &AtomicBool,
        progress: &AtomicU64,
    ) -> StudioResult<()> {
        let mut writer = frame_mux::MkvRasterWriter::create(output, info.width, info.height)?;
        let mut offset = 0;
        let mut count = 0;
        for clip in &timeline.clips {
            let source = &info.segments[clip.source_index];
            let first = source
                .frames
                .partition_point(|(t, _)| t.0 <= clip.in_ns)
                .saturating_sub(1);
            let end = source.frames.partition_point(|(t, _)| t.0 < clip.out_ns);
            let filter = format!(
                "select=between(n\\,{first}\\,{}),{}",
                end - 1,
                crate::colour::normalize_filter(source, info, true)?
            );
            let mut decoder = Process(
                command(&self.ffmpeg)
                    .args(["-v", "error", "-xerror", "-nostdin", "-noautorotate", "-i"])
                    .arg(&timeline.sources[clip.source_index].path)
                    .args([
                        "-map",
                        "0:v:0",
                        "-an",
                        "-vf",
                        &filter,
                        "-fps_mode",
                        "passthrough",
                        "-pix_fmt",
                        "rgb48le",
                        "-f",
                        "rawvideo",
                        "pipe:1",
                    ])
                    .stdout(Stdio::piped())
                    .stderr(File::create(scratch.join("decode16.log")).map_err(|e| e.to_string())?)
                    .spawn()
                    .map_err(|e| e.to_string())?,
            );
            let mut stdout = decoder.0.stdout.take().ok_or("Decoder pipe unavailable.")?;
            let mut pixels = vec![0; info.width as usize * info.height as usize * 6];
            for index in first..end {
                check_cancel(cancel)?;
                stdout.read_exact(&mut pixels).map_err(|e| {
                    format!(
                        "High precision decode stopped: {e}. {}",
                        File::open(scratch.join("decode16.log")).ok().map_or_else(
                            String::new,
                            |log| {
                                let mut bytes = Vec::new();
                                let _ = log.take(16 * 1024).read_to_end(&mut bytes);
                                String::from_utf8_lossy(&bytes).into_owned()
                            }
                        )
                    )
                })?;
                let at = offset + source.frames[index].0.0.max(clip.in_ns) - clip.in_ns;
                let until = if index + 1 < end {
                    offset + source.frames[index + 1].0.0 - clip.in_ns
                } else {
                    offset + clip.duration_ns()
                };
                let rgb = crate::colour::render_rgb16(timeline, at, info, &pixels)?;
                writer.write(
                    MediaTimestamp(at),
                    until - at,
                    &crate::render::encode_png_depth(
                        info.width,
                        info.height,
                        &rgb,
                        png::BitDepth::Sixteen,
                    )?,
                )?;
                count += 1;
                progress.store(count, Ordering::Relaxed);
            }
            if !decoder.0.wait().map_err(|e| e.to_string())?.success() {
                return Err("High precision decode failed. No output published.".into());
            }
            offset += clip.duration_ns();
        }
        writer.finish()
    }
    pub(crate) fn configure_video_encoder(
        &self,
        cmd: &mut Command,
        profile: ExportProfile,
        info: &MediaInfo,
        preserving: bool,
    ) -> StudioResult<()> {
        let pix = if preserving {
            info.pixel_format.as_str()
        } else {
            "bgr0"
        };
        match profile {
            ExportProfile::H264 { crf } => {
                cmd.args([
                    "-c:v",
                    "libx264",
                    "-preset",
                    "medium",
                    "-crf",
                    &crf.to_string(),
                    "-pix_fmt",
                    "yuv420p",
                ]);
            }
            ExportProfile::Lossless => {
                cmd.args(["-c:v", "ffv1", "-level", "3", "-pix_fmt", pix]);
            }
            ExportProfile::Hevc { crf } => {
                cmd.args([
                    "-c:v",
                    "libx265",
                    "-preset",
                    "medium",
                    "-crf",
                    &crf.to_string(),
                    "-pix_fmt",
                    if info.bit_depth == 10 {
                        "yuv420p10le"
                    } else {
                        "yuv420p"
                    },
                ]);
            }
            ExportProfile::Av1 { crf } => {
                cmd.args([
                    "-c:v",
                    "libsvtav1",
                    "-preset",
                    "8",
                    "-crf",
                    &crf.to_string(),
                    "-pix_fmt",
                    if info.bit_depth == 10 {
                        "yuv420p10le"
                    } else {
                        "yuv420p"
                    },
                ]);
            }
        }
        if !preserving && !matches!(profile, ExportProfile::Lossless) {
            // The raster intermediate contains full-range RGB. Do not let the
            // scaler select a resolution-dependent matrix for any SDR codec.
            cmd.args([
                "-vf",
                "scale=out_color_matrix=bt709:out_range=tv",
                "-colorspace",
                "bt709",
                "-color_range",
                "tv",
            ]);
        }
        for (key, value) in [
            ("-color_primaries", &info.colour_primaries),
            ("-color_trc", &info.colour_transfer),
        ] {
            if let Some(v) = value {
                cmd.args([key, v]);
            }
        }
        if preserving {
            for (key, value) in [
                ("-colorspace", &info.colour_space),
                ("-color_range", &info.colour_range),
            ] {
                if let Some(v) = value {
                    cmd.args([key, v]);
                }
            }
        }
        let mut params = Vec::new();
        for data in &info.hdr_metadata {
            if data["side_data_type"] == "Mastering display metadata" {
                let value = |key: &str, units: f64| -> StudioResult<u64> {
                    let s = data[key].as_str().ok_or("Missing mastering metadata.")?;
                    let (a, b) = s.split_once('/').ok_or("Invalid mastering metadata.")?;
                    let a = a.parse::<f64>().map_err(|e| e.to_string())?;
                    let b = b.parse::<f64>().map_err(|e| e.to_string())?;
                    if !a.is_finite() || !b.is_finite() || b <= 0.0 || a < 0.0 {
                        return Err("Invalid mastering metadata.".into());
                    }
                    Ok((a / b * units).round() as u64)
                };
                let mastering = format!(
                    "G({},{})B({},{})R({},{})WP({},{})L({},{})",
                    value("green_x", 50000.)?,
                    value("green_y", 50000.)?,
                    value("blue_x", 50000.)?,
                    value("blue_y", 50000.)?,
                    value("red_x", 50000.)?,
                    value("red_y", 50000.)?,
                    value("white_point_x", 50000.)?,
                    value("white_point_y", 50000.)?,
                    value("max_luminance", 10000.)?,
                    value("min_luminance", 10000.)?
                );
                params.push(format!("master-display={mastering}"));
            } else if data["side_data_type"] == "Content light level metadata" {
                params.push(format!(
                    "{}={},{}",
                    if matches!(profile, ExportProfile::Av1 { .. }) {
                        "content-light"
                    } else {
                        "max-cll"
                    },
                    data["max_content"].as_u64().ok_or("Missing MaxCLL.")?,
                    data["max_average"].as_u64().ok_or("Missing MaxFALL.")?
                ));
            }
        }
        if !params.is_empty() {
            match profile {
                ExportProfile::Hevc{..} => { cmd.args(["-x265-params",&params.join(":")]); }
                ExportProfile::Av1{..} => { cmd.args(["-svtav1-params",&params.join(":")]); }
                ExportProfile::Lossless => return Err("FFV1 export cannot preserve supplied mastering/light metadata in this path. Select HEVC or AV1.".into()),
                _=>(),
            }
        }
        Ok(())
    }
}

/// Native planar YUV sampling: no RGB conversion, quantization to 8-bit or tone map.
fn zoom_filter(timeline: &Timeline, offset: u64) -> String {
    let t = format!("(T+{})", seconds(offset));
    let mut size = String::from("1");
    let mut left = String::from("0");
    let mut top = String::from("0");
    for z in timeline.zooms.iter().rev().filter(|z| z.enabled) {
        let q = format!(
            "clip(min({t}-{},{}-{t})/{},0,1)",
            seconds(z.start_ns),
            seconds(z.end_ns),
            seconds(z.ramp_ns)
        );
        let weight = match z.easing {
            Easing::Linear => q.clone(),
            Easing::Smooth => format!("({q})*({q})*({q})*(10+({q})*(-15+6*({q})))"),
        };
        let s = format!("1/(1+{}*({weight}))", z.scale - 1.0);
        let condition = format!(
            "gte({t},{})*lt({t},{})",
            seconds(z.start_ns),
            seconds(z.end_ns)
        );
        let x = (z.focus.x - 0.5 / z.scale).clamp(0., 1. - 1. / z.scale);
        let y = (z.focus.y - 0.5 / z.scale).clamp(0., 1. - 1. / z.scale);
        size = format!("if({condition},{s},{size})");
        left = format!("if({condition},min({x}*({weight}),1-({s})),{left})");
        top = format!("if({condition},min({y}*({weight}),1-({s})),{top})");
    }
    let x = format!("clip(({left})*W+(X+0.5)*({size})-0.5,0,W-1)");
    let y = format!("clip(({top})*H+(Y+0.5)*({size})-0.5,0,H-1)");
    format!("geq=lum='lum({x},{y})':cb='cb({x},{y})':cr='cr({x},{y})':interpolation=bilinear")
}
impl LocalTools {
    fn render_preserving(
        &self,
        timeline: &Timeline,
        info: &MediaInfo,
        output: &Path,
        scratch: &Path,
        cancel: &AtomicBool,
        progress: &AtomicU64,
    ) -> StudioResult<()> {
        let caps = self.capabilities()?;
        if !caps.encoders.contains("ffv1")
            || (timeline.zooms.iter().any(|z| z.enabled) && !caps.filters.contains("geq"))
        {
            return Err("HDR rendering requires FFV1 and geq in the local FFmpeg runtime.".into());
        }
        // Per-clip intermediates preserve VFR. A final frame holds restart gaps.
        let mut parts = Vec::new();
        let mut offset = 0u64;
        for (i, clip) in timeline.clips.iter().enumerate() {
            let source = &info.segments[clip.source_index];
            let first = source
                .frames
                .partition_point(|(t, _)| t.0 <= clip.in_ns)
                .saturating_sub(1);
            let end = source.frames.partition_point(|(t, _)| t.0 < clip.out_ns);
            let first_pts = source.frames[first].0.0.max(clip.in_ns) - clip.in_ns;
            let path = scratch.join(format!("native-{i}.mkv"));
            let graph = format!(
                "select=between(n\\,{first}\\,{}),settb=1/1000000,setpts='max(PTS-{}/TB,0)',{}{}",
                end.saturating_sub(1),
                seconds(clip.in_ns),
                crate::colour::normalize_filter(source, info, false)?,
                if timeline.zooms.iter().any(|z| z.enabled) {
                    format!(",{}", zoom_filter(timeline, offset))
                } else {
                    String::new()
                }
            );
            let script = scratch.join(format!("native-{i}.filter"));
            fs::write(&script, &graph).map_err(|e| e.to_string())?;
            let mut cmd = command(&self.ffmpeg);
            cmd.args([
                "-v",
                "error",
                "-xerror",
                "-nostdin",
                "-n",
                "-copyts",
                "-noautorotate",
                "-i",
            ])
            .arg(&timeline.sources[clip.source_index].path)
            .args(["-map", "0:v:0", "-an", "-filter_script:v"])
            .arg(script)
            .args([
                "-c:v",
                "ffv1",
                "-level",
                "3",
                "-pix_fmt",
                &info.pixel_format,
                "-fps_mode",
                "passthrough",
                "-enc_time_base:v",
                "1:1000000",
                "-avoid_negative_ts",
                "disabled",
            ]);
            for (key, value) in [
                ("-color_primaries", &info.colour_primaries),
                ("-color_trc", &info.colour_transfer),
                ("-colorspace", &info.colour_space),
                ("-color_range", &info.colour_range),
            ] {
                if let Some(v) = value {
                    cmd.args([key, v]);
                }
            }
            cmd.arg(&path);
            run(cmd, &scratch.join("native.log"), cancel)?;
            // ffconcat duration controls the next segment origin even when its last
            // picture is held over an encoder restart gap. Keep initial video offset.
            parts.push((path, clip.duration_ns(), first_pts));
            offset += clip.duration_ns();
            progress.store(
                info.frames.partition_point(|(t, _)| t.0 < offset) as u64,
                Ordering::Relaxed,
            );
        }
        let list = scratch.join("native.txt");
        let mut text = String::from("ffconcat version 1.0\n");
        for (path, duration, _) in &parts {
            let p = path
                .to_string_lossy()
                .replace('\\', "/")
                .replace('\'', "'\\''");
            text.push_str(&format!(
                "file '{p}'\ninpoint 0\nduration {}\n",
                seconds(*duration)
            ));
        }
        fs::write(&list, text).map_err(|e| e.to_string())?;
        let mut cmd = command(&self.ffmpeg);
        cmd.args([
            "-v", "error", "-nostdin", "-n", "-copyts", "-f", "concat", "-safe", "0", "-i",
        ])
        .arg(&list)
        .args([
            "-map",
            "0:v",
            "-c:v",
            "copy",
            "-avoid_negative_ts",
            "disabled",
        ])
        .arg(output);
        run(cmd, &scratch.join("native-join.log"), cancel)
    }
}
