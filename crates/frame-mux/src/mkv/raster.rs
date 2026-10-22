//! Lossless rendered frames on the same timestamp/EBML path as recordings.
//! Studio supplies PNG pictures; this intermediate never replaces source media.
use super::{
    backend::Backend,
    output::{OutputFile, SinkAdapter},
};
use frame_clock::MediaTimestamp;
use mkwriter::{Frame, TrackEntry, Tracks, VideoSettings};
use std::{fs::OpenOptions, path::Path};

pub struct MkvRasterWriter {
    backend: Backend,
    previous: Option<u64>,
}
impl MkvRasterWriter {
    pub fn create(path: &Path, width: u32, height: u32) -> Result<Self, String> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| e.to_string())?;
        // BITMAPINFOHEADER: MPNG is the standard Matroska VFW PNG mapping.
        let mut private = vec![0u8; 40];
        private[0..4].copy_from_slice(&40u32.to_le_bytes());
        private[4..8].copy_from_slice(&width.to_le_bytes());
        private[8..12].copy_from_slice(&height.to_le_bytes());
        private[12..14].copy_from_slice(&1u16.to_le_bytes());
        private[14..16].copy_from_slice(&24u16.to_le_bytes());
        private[16..20].copy_from_slice(b"MPNG");
        let tracks = Tracks {
            entries: vec![
                TrackEntry::new(1, 1, mkwriter::ids::track_type::VIDEO, "V_MS/VFW/FOURCC")
                    .with_codec_private(private)
                    .video(VideoSettings::new(width as u64, height as u64)),
            ],
        };
        let backend = Backend::new(
            SinkAdapter(Box::new(OutputFile::new(file).map_err(|e| e.to_string())?)),
            &super::container_info(),
            &tracks,
        )
        .map_err(|e| e.to_string())?;
        Ok(Self {
            backend,
            previous: None,
        })
    }
    pub fn write(
        &mut self,
        pts: MediaTimestamp,
        duration_ns: u64,
        png: &[u8],
    ) -> Result<(), String> {
        if duration_ns < super::TIMESTAMP_SCALE_NS
            || self
                .previous
                .is_some_and(|p| pts.0 / super::TIMESTAMP_SCALE_NS <= p)
        {
            return Err(
                "Rendered frame timestamps must be strictly increasing at container precision."
                    .into(),
            );
        }
        if pts.0.checked_add(duration_ns).is_none() {
            return Err("Rendered timestamp overflow.".into());
        }
        self.backend
            .write_frame(
                Frame {
                    track: 1,
                    timestamp_ns: pts.0,
                    duration_ns: Some(duration_ns),
                    data: png,
                    keyframe: true,
                    ..Default::default()
                },
                false,
            )
            .map_err(|e| e.to_string())?;
        self.previous = Some(pts.0 / super::TIMESTAMP_SCALE_NS);
        Ok(())
    }
    pub fn finish(self) -> Result<(), String> {
        if self.previous.is_none() {
            return Err("No rendered frames.".into());
        }
        let target = self
            .backend
            .finalize(&[1], None)
            .map_err(|e| e.to_string())?;
        target.0.finish().map_err(|e| e.to_string())
    }
}
