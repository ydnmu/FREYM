use crate::*;
use frame_clock::MediaTimestamp;
use std::{
    fs,
    path::Path,
    process::Child,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

/// Bilinear sampling retains the source geometry; editing never changes source PTS.
pub fn render_rgb(
    timeline: &Timeline,
    time: MediaTimestamp,
    width: u32,
    height: u32,
    source: &[u8],
) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    assert_eq!(source.len(), w * h * 3);
    let view = timeline.viewport(time.0);
    let mut output = vec![0u8; source.len()];
    if view.width == 1.0 {
        output.copy_from_slice(source);
    } else {
        for y in 0..h {
            let sy = ((view.top + (y as f64 + 0.5) / h as f64 * view.height) * h as f64 - 0.5)
                .clamp(0.0, (h - 1) as f64);
            let y0 = sy.floor() as usize;
            let y1 = (y0 + 1).min(h - 1);
            let dy = sy - y0 as f64;
            for x in 0..w {
                let sx = ((view.left + (x as f64 + 0.5) / w as f64 * view.width) * w as f64 - 0.5)
                    .clamp(0.0, (w - 1) as f64);
                let x0 = sx.floor() as usize;
                let x1 = (x0 + 1).min(w - 1);
                let dx = sx - x0 as f64;
                for c in 0..3 {
                    let a = source[(y0 * w + x0) * 3 + c] as f64 * (1.0 - dx)
                        + source[(y0 * w + x1) * 3 + c] as f64 * dx;
                    let b = source[(y1 * w + x0) * 3 + c] as f64 * (1.0 - dx)
                        + source[(y1 * w + x1) * 3 + c] as f64 * dx;
                    output[(y * w + x) * 3 + c] = (a * (1.0 - dy) + b * dy).round() as u8;
                }
            }
        }
    }
    paint_cursor(timeline, time.0, width, height, |pixel, colour, alpha| {
        for c in 0..3 {
            output[pixel * 3 + c] =
                (output[pixel * 3 + c] as f64 * (1.0 - alpha) + colour[c] * alpha) as u8;
        }
    });
    output
}
pub(crate) fn paint_cursor(
    timeline: &Timeline,
    time: u64,
    width: u32,
    height: u32,
    mut paint: impl FnMut(usize, [f64; 3], f64),
) {
    let w = width as usize;
    let h = height as usize;
    let view = timeline.viewport(time);
    if timeline.cursor.enabled
        && let Some((cursor, clicked)) = timeline.cursor_at(time)
    {
        let cx = (cursor.x - view.left) / view.width * w as f64;
        let cy = (cursor.y - view.top) / view.height * h as f64;
        let size = timeline.cursor.size * (width as f64 / 1920.0).max(0.75);
        let baked = timeline
            .locate(time)
            .and_then(|(i, _, _)| timeline.sources.get(timeline.clips[i].source_index))
            .and_then(|s| s.metadata.as_ref())
            .or(timeline.metadata.as_ref())
            .is_some_and(|m| m.cursor_baked);
        let radius = if clicked { 22.0 } else { 15.0 } * size;
        let bounds = radius.max(25.0 * size).ceil() as i32;
        for y in (cy as i32 - bounds).max(0)..=(cy as i32 + bounds).min(height as i32 - 1) {
            for x in (cx as i32 - bounds).max(0)..=(cx as i32 + bounds).min(width as i32 - 1) {
                let dx = x as f64 - cx;
                let dy = y as f64 - cy;
                let distance = dx.hypot(dy);
                let at = y as usize * w + x as usize;
                if timeline.cursor.emphasis
                    && distance < radius
                    && (clicked || distance > radius - 2.5 * size)
                {
                    let alpha = if clicked { 0.35 } else { 0.65 };
                    paint(at, [255.0, 198.0, 50.0], alpha);
                }
                // A geometric pointer is drawn only when source pixels have no cursor.
                if !baked && dy >= 0.0 && dy <= 20.0 * size && dx >= 0.0 && dx <= dy * 0.55 {
                    let value =
                        if dx < 1.5 * size || dy - dx / 0.55 < 2.0 * size || dy > 18.0 * size {
                            20
                        } else {
                            255
                        };
                    paint(at, [value as f64; 3], 1.0);
                }
            }
        }
    }
}
pub fn encode_png(width: u32, height: u32, rgb: &[u8]) -> StudioResult<Vec<u8>> {
    encode_png_depth(width, height, rgb, png::BitDepth::Eight)
}
pub(crate) fn encode_png_depth(
    width: u32,
    height: u32,
    rgb: &[u8],
    depth: png::BitDepth,
) -> StudioResult<Vec<u8>> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(depth);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(rgb).map_err(|e| e.to_string())?;
    }
    Ok(bytes)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportProfile {
    H264 { crf: u8 },
    Lossless,
    Hevc { crf: u8 },
    Av1 { crf: u8 },
}
impl Default for ExportProfile {
    fn default() -> Self {
        Self::H264 { crf: 18 }
    }
}
#[derive(Clone, Debug)]
pub struct ExportReport {
    pub frames: u64,
    pub audio_tracks: usize,
    pub duration_ns: u64,
    pub method: ExportMethod,
    pub copied_video_frames: u64,
    pub reencoded_video_frames: u64,
    /// Explains a rejected packet/hybrid optimization, when full rendering was used.
    pub fallback_reason: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportMethod {
    /// Encoded packets preserved; does not imply the source encoder was lossless.
    StreamCopy,
    /// Boundary video or sample-accurate independent audio was encoded.
    SmartRender,
    FullRender,
}
impl ExportMethod {
    pub fn label(self) -> &'static str {
        match self {
            Self::StreamCopy => "Stream Copy",
            Self::SmartRender => "Smart Render",
            Self::FullRender => "Full Render",
        }
    }
}
pub(crate) struct Process(pub(crate) Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
pub(crate) struct Workspace(pub(crate) std::path::PathBuf);
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
pub(crate) fn check_cancel(cancel: &AtomicBool) -> StudioResult<()> {
    if cancel.load(Ordering::Relaxed) {
        Err("Export cancelled; source media and edit decisions are preserved.".into())
    } else {
        Ok(())
    }
}
impl LocalTools {
    /// Automatically preserve compatible packets before using the existing renderer.
    pub fn export(
        &self,
        timeline: &Timeline,
        output: &Path,
        profile: ExportProfile,
        cancel: &AtomicBool,
        progress: &AtomicU64,
    ) -> StudioResult<ExportReport> {
        timeline.verify_source()?;
        // One export admission/publication path for legacy and current edit JSON.
        self.export_assembled(timeline, output, profile, cancel, progress)
    }
}
