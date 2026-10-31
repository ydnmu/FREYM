//! Opt-in disk archive over the existing packet buffer and segment manifest.
//! Only an exclusively created directory belongs to this store. Retirement is
//! durably journaled before atomic removal of references and verified deletion;
//! an I/O failure stops rotation and preserves offline recovery evidence.
use super::*;
use crate::{RecordingSession, SessionManifest, SessionRecoverySummary};

#[derive(Debug, Clone, Copy)]
pub struct DiskReplayLimit {
    pub segment_ns: u64,
    pub history_ns: u64,
    pub max_bytes: u64,
    pub max_segments: usize,
}
impl DiskReplayLimit {
    fn validate(self) -> Result<Self, RecorderError> {
        if !(1_000_000_000..=60_000_000_000).contains(&self.segment_ns)
            || !(self.segment_ns..=600_000_000_000).contains(&self.history_ns)
            || !(16 * 1024 * 1024..=16 * 1024 * 1024 * 1024).contains(&self.max_bytes)
            || !(1..=crate::session::MAX_SESSION_SEGMENTS).contains(&self.max_segments)
        {
            return Err(RecorderError::invalid(
                "Invalid replay segment/history/disk/count budget.",
            ));
        }
        Ok(self)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DiskReplayStats {
    pub sealed_segments: u64,
    pub retired_segments: u64,
    pub retained_segments: usize,
    /// Sum of file lengths, including manifest/journal/temp overhead. Filesystem
    /// allocation/compression and volume-wide free space are separate facts.
    pub disk_bytes: u64,
    pub last_sealed_end_ns: Option<u64>,
}

pub struct RollingReplayStore {
    directory: PathBuf,
    _ownership: crate::session::retirement::ArchiveOwnership,
    session: RecordingSession,
    limit: DiskReplayLimit,
    stats: DiskReplayStats,
    failure: Option<RecorderError>,
}
impl RollingReplayStore {
    /// The directory must be absolute and new. No existing recording or arbitrary
    /// directory is adopted for automatic deletion. After restart, `recover_owned`
    /// can clean up retirement and export clips without resuming capture.
    pub fn create(directory: &Path, limit: DiskReplayLimit) -> Result<Self, RecorderError> {
        let limit = limit.validate()?;
        if !directory.is_absolute() {
            return Err(RecorderError::invalid("Replay directory must be absolute."));
        }
        std::fs::create_dir(directory).map_err(io_error)?;
        let directory = directory.canonicalize().map_err(io_error)?;
        let ownership =
            crate::session::retirement::ArchiveOwnership::create(&directory).map_err(io_error)?;
        let mut session = RecordingSession::create(&directory.join("replay.freym-session"))?;
        session.enable_rolling()?;
        Ok(Self {
            directory,
            _ownership: ownership,
            session,
            limit,
            stats: DiskReplayStats::default(),
            failure: None,
        })
    }
    pub fn manifest_path(&self) -> &Path {
        self.session.manifest_path()
    }
    pub fn stats(&self) -> DiskReplayStats {
        self.stats
    }
    pub fn segment_duration_ns(&self) -> u64 {
        self.limit.segment_ns
    }
    pub fn snapshot(&self) -> Result<SessionManifest, RecorderError> {
        self.session.snapshot()
    }

    /// Called by the recording control loop, after packet delivery. It never
    /// creates a duplicate clip for an unchanged source. Native/preview threads
    /// do no filesystem work. Sync/CRC costs are intentionally opt-in.
    pub fn checkpoint_if_due(&mut self, buffer: &ReplayBuffer) -> Result<bool, RecorderError> {
        if let Some(error) = self.failure.as_ref() {
            return Err(error.clone());
        }
        let Some(end) = buffer.shared_end_ns() else {
            return Ok(false);
        };
        if !buffer.has_start(self.limit.segment_ns)
            || self
                .stats
                .last_sealed_end_ns
                .is_some_and(|last| end < last.saturating_add(self.limit.segment_ns))
        {
            return Ok(false);
        }
        let result = self.seal(buffer, end);
        if let Err(error) = result.as_ref() {
            self.failure = Some(error.clone());
        }
        result.map(|()| true)
    }
    /// Called only by replay I/O. The exclusively owned archive contains one
    /// bounded sidecar, replaced after each verified seal/retirement. Its events
    /// have exactly the retained segment range, including keyframe overlap.
    pub fn checkpoint_with_metadata(
        &mut self,
        buffer: &ReplayBuffer,
        events: &frame_metadata::RollingMetadata,
    ) -> Result<bool, RecorderError> {
        let changed = self.checkpoint_if_due(buffer)?;
        if changed {
            let manifest = self.session.snapshot()?;
            let start = manifest
                .segments
                .first()
                .ok_or_else(|| RecorderError::invalid("Empty replay archive."))?
                .origin_ns;
            let end = manifest
                .segments
                .iter()
                .map(|s| s.origin_ns.saturating_add(s.container_end_ns))
                .max()
                .unwrap();
            let snapshot = events.pin(start, end).map_err(io_error)?;
            let destination = self.session.metadata_path();
            let temporary = self.directory.join("events.pending");
            snapshot.write(&temporary).map_err(io_error)?;
            let size = std::fs::metadata(&temporary).map_err(io_error)?.len();
            let old_size = std::fs::metadata(&destination).map_or(0, |m| m.len());
            if self.disk_usage()?.saturating_sub(old_size) > self.limit.max_bytes {
                let _ = std::fs::remove_file(&temporary);
                return Err(RecorderError::invalid(format!(
                    "Replay metadata ({size} bytes) exceeds the disk budget; media preserved, rotation stopped."
                )));
            }
            if destination.exists()
                && !std::fs::symlink_metadata(&destination).is_ok_and(|m| m.is_file())
            {
                return Err(RecorderError::invalid(
                    "Replay metadata destination is not a regular owned file.",
                ));
            }
            std::fs::rename(&temporary, &destination).map_err(io_error)?;
            if manifest.metadata_file.is_none() {
                self.session.attach_metadata()?;
            }
            events.retain_from(start.min(buffer.retained_start_ns().unwrap_or(start)));
            self.stats.disk_bytes = self.disk_usage()?;
        }
        Ok(changed)
    }

    fn seal(&mut self, buffer: &ReplayBuffer, end: u64) -> Result<(), RecorderError> {
        // Coalesced background checkpoints must cover the interval since the
        // preceding seal. A backlog longer than retained history is explicit.
        let duration = self
            .stats
            .last_sealed_end_ns
            .map_or(self.limit.segment_ns, |last| {
                self.limit.segment_ns.max(end.saturating_sub(last))
            });
        if duration
            > buffer
                .limit()
                .duration_ns
                .saturating_add(KEYFRAME_LOOKBACK_NS)
        {
            return Err(RecorderError::invalid(
                "Replay disk worker fell behind retained history; archive preserved, rotation stopped.",
            ));
        }
        if !buffer.has_start(duration) {
            return Err(RecorderError::invalid(
                "Replay lost the beginning of an unsealed interval; archive preserved, rotation stopped.",
            ));
        }
        let reserve = buffer.disk_reservation_bytes(duration);
        if reserve > self.limit.max_bytes {
            return Err(RecorderError::invalid(
                "Replay memory window cannot fit the disk reservation budget.",
            ));
        }
        let manifest = self.session.snapshot()?;
        let cutoff = end.saturating_sub(self.limit.history_ns);
        let mut retired = manifest
            .segments
            .iter()
            .take_while(|s| s.origin_ns.saturating_add(s.container_end_ns) <= cutoff)
            .count();
        let mut remaining = manifest.segments.len() - retired;
        let mut reclaim = manifest
            .segments
            .iter()
            .take(retired)
            .map(|s| s.file_bytes)
            .sum::<u64>();
        let usage = self.disk_usage()?;
        while remaining >= self.limit.max_segments
            || usage.saturating_sub(reclaim).saturating_add(reserve) > self.limit.max_bytes
        {
            let Some(segment) = manifest.segments.get(retired) else {
                return Err(RecorderError::invalid(
                    "Replay disk metadata/orphan files exceed the budget; rotation stopped.",
                ));
            };
            reclaim = reclaim.saturating_add(segment.file_bytes);
            retired += 1;
            remaining -= 1;
        }
        if retired != 0 {
            self.session.retire_replay_prefix(retired)?;
            self.stats.retired_segments += retired as u64;
        }
        let path = self.session.next_segment_path()?;
        buffer.save_internal(&path, duration, None, Some(&mut self.session))?;
        let measured = self.disk_usage()?;
        if measured > self.limit.max_bytes {
            return Err(RecorderError::invalid(
                "Replay exceeded its conservative disk reservation; files preserved, rotation stopped.",
            ));
        }
        self.stats.sealed_segments += 1;
        self.stats.last_sealed_end_ns = Some(end);
        self.stats.retained_segments = self.session.snapshot()?.segments.len();
        self.stats.disk_bytes = measured;
        Ok(())
    }

    fn disk_usage(&self) -> Result<u64, RecorderError> {
        let mut bytes = 0u64;
        for (index, entry) in std::fs::read_dir(&self.directory)
            .map_err(io_error)?
            .enumerate()
        {
            if index > crate::session::MAX_SESSION_SEGMENTS + 8 {
                return Err(RecorderError::invalid(
                    "Unexpected file count in the owned replay directory.",
                ));
            }
            let entry = entry.map_err(io_error)?;
            let meta = std::fs::symlink_metadata(entry.path()).map_err(io_error)?;
            if !meta.is_file() {
                return Err(RecorderError::invalid(
                    "Replay directory contains a non-regular file.",
                ));
            }
            bytes = bytes
                .checked_add(meta.len())
                .ok_or_else(|| RecorderError::invalid("Replay disk byte overflow."))?;
        }
        Ok(bytes)
    }

    /// Copy independently playable, verified segments through existing recovery;
    /// each segment retains its own Tracks/codec/Opus trim. No mixed-track merge.
    pub fn export(
        &mut self,
        output_manifest: &Path,
    ) -> Result<SessionRecoverySummary, RecorderError> {
        if let Some(error) = self.failure.as_ref() {
            return Err(error.clone());
        }
        // A live session's locked journal intentionally prevents recovery. Copy
        // its sealed snapshot using the same verification path under this owner.
        crate::session::export_closed_snapshot(self.session.manifest_path(), output_manifest)
    }

    /// Explicitly recover an abandoned, marked replay archive. Pending retirement
    /// is completed only under archive and session journal locks; the verified
    /// retained clips are exported independently through existing session logic.
    /// This never resumes capture or adopts a new shared clock into the archive.
    pub fn recover_owned(
        directory: &Path,
        output_manifest: &Path,
    ) -> Result<SessionRecoverySummary, RecorderError> {
        crate::session::retirement::recover_owned(directory, output_manifest)
    }
}

fn io_error(error: std::io::Error) -> RecorderError {
    RecorderError::invalid(format!(
        "Replay disk I/O failed; preserve its recovery files: {error}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use frame_encode::{BitstreamFormat, EncoderCapability, VideoColour};
    use std::num::NonZeroU32;

    #[test]
    fn interrupted_background_seal_recovers_its_checkpoint_and_preserves_closed_media() {
        let root = std::env::temp_dir().join("freym-interrupted-seal");
        let directory = root.join(format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let data = include_bytes!("../../tests/fixtures/synthetic-main10.hevc").to_vec();
        let stream = EncodedStreamInfo {
            capability: EncoderCapability {
                id: "hevc-fixture".into(),
                codec: "hevc".into(),
                hardware: false,
            },
            width: NonZeroU32::new(64).unwrap(),
            height: NonZeroU32::new(64).unwrap(),
            nominal_rate_num: NonZeroU32::new(1).unwrap(),
            nominal_rate_den: NonZeroU32::new(1).unwrap(),
            bitstream_format: BitstreamFormat::HevcAnnexB,
            colour: Some(VideoColour::BT2020_PQ_LIMITED),
            codec_config: data.clone(),
            allows_reordering: false,
        };
        let mut buffer = ReplayBuffer::new(
            stream.clone(),
            vec![],
            ReplayLimit {
                duration_ns: 1_000_000_000,
                max_bytes: 1024 * 1024,
            },
        )
        .unwrap();
        let packet = |pts| EncodedPacket {
            track_id: 0,
            pts: MediaTimestamp(pts),
            dts: MediaTimestamp(pts),
            duration_ns: 1_000_000_000,
            keyframe: true,
            data: data.clone(),
        };
        buffer.push_video(&packet(0));
        let mut store = RollingReplayStore::create(
            &directory.join("owned"),
            DiskReplayLimit {
                segment_ns: 1_000_000_000,
                history_ns: 2_000_000_000,
                max_bytes: 16 * 1024 * 1024,
                max_segments: 2,
            },
        )
        .unwrap();
        store.checkpoint_if_due(&buffer).unwrap();
        let closed = store.snapshot().unwrap().segments[0].clone();
        let closed_path = directory.join("owned").join(&closed.file_name);
        let original = std::fs::read(&closed_path).unwrap();
        let mut tail = store
            .session
            .create_video_segment(&stream, MediaTimestamp(1_000_000_000))
            .unwrap();
        tail.write(packet(1_000_000_000)).unwrap();
        tail.checkpoint().unwrap();
        drop(tail);
        drop(store);
        let recovered = RollingReplayStore::recover_owned(
            &directory.join("owned"),
            &directory.join("recovered.freym-session"),
        )
        .unwrap();
        assert_eq!(recovered.verified_closed_segments, 1);
        assert_eq!(recovered.recovered_tails.len(), 1);
        assert!(!recovered.manifest.completed);
        assert!(recovered.rejected_segments.is_empty());
        assert_eq!(std::fs::read(closed_path).unwrap(), original);
        for segment in recovered.manifest.segments {
            let decoded = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-xerror", "-i"])
                .arg(directory.join(segment.file_name))
                .args(["-f", "null", "-"])
                .output()
                .unwrap();
            assert!(
                decoded.status.success(),
                "{}",
                String::from_utf8_lossy(&decoded.stderr)
            );
        }
    }
}
