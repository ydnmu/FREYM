//! Studio-only runtime detection. The recorder never consults these tools.
use crate::*;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct ToolCapabilities {
    pub version: String,
    pub encoders: BTreeSet<String>,
    pub filters: BTreeSet<String>,
    pub decoders: BTreeSet<String>,
    pub muxers: BTreeSet<String>,
    pub demuxers: BTreeSet<String>,
}
impl LocalTools {
    pub fn capabilities(&self) -> StudioResult<ToolCapabilities> {
        let get = |program: &Path, args: &[&str]| -> StudioResult<String> {
            let mut cmd = command(program);
            cmd.args(args);
            Ok(String::from_utf8_lossy(&crate::media::probe_output(cmd)?).into_owned())
        };
        let version = get(&self.ffmpeg, &["-version"])?;
        let probe = get(&self.ffprobe, &["-version"])?;
        let major = |s: &str| {
            s.split_whitespace()
                .nth(2)
                .and_then(|s| s.split('.').next())
                .and_then(|s| s.parse::<u32>().ok())
        };
        if major(&version).is_none_or(|v| v < 6) || major(&version) != major(&probe) {
            return Err("Studio requires matching FFmpeg/ffprobe 6 or newer. Configure FREYM_FFMPEG and FREYM_FFPROBE from the same installation.".into());
        }
        let names = |s: String| -> BTreeSet<String> {
            s.lines()
                .filter_map(|l| {
                    let mut parts = l.split_whitespace();
                    let flags = parts.next()?;
                    if !matches!(flags.len(), 1 | 2 | 3 | 6)
                        || !flags.chars().all(|c| c == '.' || c.is_ascii_uppercase())
                    {
                        return None;
                    }
                    let mut name = parts.next()?;
                    // Newer format listings add a lower-case device column.
                    if matches!(name, "d" | "e" | "de") {
                        name = parts.next()?;
                    }
                    (name != "=").then(|| name.to_owned())
                })
                .collect()
        };
        let encoders = names(get(&self.ffmpeg, &["-hide_banner", "-encoders"])?);
        let filters = names(get(&self.ffmpeg, &["-hide_banner", "-filters"])?);
        let decoders = names(get(&self.ffmpeg, &["-hide_banner", "-decoders"])?);
        let muxers = names(get(&self.ffmpeg, &["-hide_banner", "-muxers"])?);
        let demuxers = names(get(&self.ffmpeg, &["-hide_banner", "-demuxers"])?);
        let devices = names(get(&self.ffmpeg, &["-hide_banner", "-devices"])?);
        for encoder in ["rawvideo", "png", "pcm_f32le"] {
            if !encoders.contains(encoder) {
                return Err(format!(
                    "Local FFmpeg is missing the required {encoder} encoder."
                ));
            }
        }
        for demuxer in ["concat"] {
            if !demuxers.contains(demuxer) {
                return Err(format!(
                    "Local FFmpeg is missing the required {demuxer} demuxer."
                ));
            }
        }
        if !devices.contains("lavfi") {
            return Err(
                "Local FFmpeg is missing the lavfi input device needed for audio silence padding."
                    .into(),
            );
        }
        for muxer in ["matroska", "rawvideo", "f32le"] {
            if !muxers.contains(muxer) {
                return Err(format!(
                    "Local FFmpeg is missing the required {muxer} muxer."
                ));
            }
        }
        for filter in [
            "trim",
            "setpts",
            "scale",
            "format",
            "atrim",
            "asetpts",
            "apad",
            "aresample",
            "anullsrc",
        ] {
            if !filters.contains(filter) {
                return Err(format!(
                    "Local FFmpeg is missing the required {filter} filter."
                ));
            }
        }
        Ok(ToolCapabilities {
            version: version.lines().next().unwrap_or_default().into(),
            encoders,
            filters,
            decoders,
            muxers,
            demuxers,
        })
    }
    pub fn require_encoder(&self, profile: ExportProfile, bits: u8) -> StudioResult<()> {
        let caps = self.capabilities()?;
        let encoder = match profile {
            ExportProfile::H264 { .. } => "libx264",
            ExportProfile::Lossless => "ffv1",
            ExportProfile::Hevc { .. } => "libx265",
            ExportProfile::Av1 { .. } => "libsvtav1",
        };
        if !caps.encoders.contains(encoder) {
            return Err(format!(
                "This FFmpeg installation does not provide {encoder}. Select an available export profile or configure a compatible local runtime."
            ));
        }
        if bits == 10 || !matches!(profile, ExportProfile::Lossless) {
            let mut cmd = command(&self.ffmpeg);
            cmd.args(["-hide_banner", "-h", &format!("encoder={encoder}")]);
            let help = String::from_utf8_lossy(&crate::media::probe_output(cmd)?).into_owned();
            let pixel_format = if bits == 10 { "yuv420p10le" } else { "yuv420p" };
            if !help.split_whitespace().any(|p| p == pixel_format) {
                return Err(format!(
                    "{encoder} does not support {bits}-bit {pixel_format} output in this installation."
                ));
            }
        }
        Ok(())
    }
}
pub(crate) fn tool_path(name: &str) -> PathBuf {
    let variable = format!("FREYM_{}", name.to_uppercase());
    if let Some(path) = std::env::var_os(variable) {
        return path.into();
    }
    if let Some(dir) = std::env::var_os("FREYM_STUDIO_RUNTIME") {
        return PathBuf::from(dir).join("bin").join(if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.into()
        });
    }
    if let Ok(exe) = std::env::current_exe() {
        let path = exe
            .parent()
            .unwrap()
            .join("studio-runtime/bin")
            .join(if cfg!(windows) {
                format!("{name}.exe")
            } else {
                name.into()
            });
        if path.is_file() {
            return path;
        }
    }
    name.into()
}
