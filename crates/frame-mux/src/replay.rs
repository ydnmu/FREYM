//! Bounded encoded-packet replay. No source surfaces, PCM, or re-encoding are retained.
use crate::{AvAudioTrack, ContainerWriter, MkvAvWriter, MkvVideoWriter};
use frame_clock::MediaTimestamp;
use frame_contracts::{RecorderError, RecordingArtifact};
use frame_encode::{EncodedPacket, EncodedStreamInfo};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod disk;
pub use disk::{DiskReplayLimit, DiskReplayStats, RollingReplayStore};

/// Bounded extra retention for the random access point preceding the selected
/// window. A longer GOP is reported as missing data instead of growing history.
const KEYFRAME_LOOKBACK_NS: u64 = 2_000_000_000;
const OPUS_PREROLL_NS: u64 = 80_000_000;

#[derive(Debug, Clone, Copy)]
pub struct ReplayLimit {
    pub duration_ns: u64,
    pub max_bytes: usize,
}

impl ReplayLimit {
    pub fn validate(self) -> Result<Self, RecorderError> {
        if !(1_000_000_000..=600_000_000_000).contains(&self.duration_ns)
            || !(1024 * 1024..=512 * 1024 * 1024).contains(&self.max_bytes)
        {
            return Err(RecorderError::invalid(
                "Replay duration must be 1–600 seconds and memory 1–512 MiB.",
            ));
        }
        Ok(self)
    }
}

#[derive(Clone)]
pub struct ReplayBuffer {
    limit: ReplayLimit,
    stream: EncodedStreamInfo,
    audio: Vec<AvAudioTrack>,
    first_audio_pts: Vec<Option<u64>>,
    // Snapshots share immutable encoded payloads; saving never copies the full
    // history on the capture owner. Each snapshot still has bounded metadata.
    packets: VecDeque<Arc<ReplayPacket>>,
    bytes: usize,
    latest_end: u64,
}

struct ReplayPacket {
    packet: EncodedPacket,
    video: bool,
}

impl ReplayBuffer {
    /// Codec instances and audio pre-skip must never share an old replay buffer.
    pub fn reset_stream(
        &mut self,
        stream: EncodedStreamInfo,
        audio: Vec<AvAudioTrack>,
    ) -> Result<(), RecorderError> {
        *self = Self::new(stream, audio, self.limit)?;
        Ok(())
    }
    pub fn new(
        stream: EncodedStreamInfo,
        audio: Vec<AvAudioTrack>,
        limit: ReplayLimit,
    ) -> Result<Self, RecorderError> {
        let first_audio_pts = vec![None; audio.len()];
        Ok(Self {
            limit: limit.validate()?,
            stream,
            audio,
            first_audio_pts,
            packets: VecDeque::new(),
            bytes: 0,
            latest_end: 0,
        })
    }

    pub fn set_limit(&mut self, limit: ReplayLimit) -> Result<(), RecorderError> {
        self.limit = limit.validate()?;
        self.evict();
        Ok(())
    }

    pub fn retained_bytes(&self) -> usize {
        self.bytes
    }

    pub fn latest_end_ns(&self) -> u64 {
        self.latest_end
    }
    pub fn retained_start_ns(&self) -> Option<u64> {
        self.packets.iter().map(|p| p.packet.pts.0).min()
    }
    pub fn limit(&self) -> ReplayLimit {
        self.limit
    }

    /// Conservative reservation includes converted NAL payloads, per-packet
    /// cluster/cue overhead, Tracks, and atomic manifest/temp/journal space.
    fn disk_reservation_bytes(&self, duration_ns: u64) -> u64 {
        let earliest = self.shared_end_ns().unwrap_or(0).saturating_sub(
            duration_ns
                .saturating_add(KEYFRAME_LOOKBACK_NS)
                .saturating_add(OPUS_PREROLL_NS),
        );
        6 * 1024 * 1024
            + self
                .packets
                .iter()
                .filter(|p| p.packet.pts.0 >= earliest)
                .map(|p| {
                    (p.packet.data.len() as u64)
                        .saturating_mul(2)
                        .saturating_add(512)
                })
                .sum::<u64>()
    }

    fn has_start(&self, duration_ns: u64) -> bool {
        self.shared_end_ns()
            .and_then(|end| end.checked_sub(duration_ns))
            .is_some_and(|start| {
                self.packets
                    .iter()
                    .any(|p| p.video && p.packet.keyframe && p.packet.pts.0 <= start)
            })
    }

    pub fn shared_end_ns(&self) -> Option<u64> {
        let mut end = self
            .packets
            .iter()
            .filter(|p| p.video)
            .map(|p| p.packet.pts.0.saturating_add(p.packet.duration_ns))
            .max()?;
        for track in 0..self.audio.len() {
            end = end.min(
                self.packets
                    .iter()
                    .filter(|p| !p.video && p.packet.track_id as usize == track)
                    .map(|p| p.packet.pts.0.saturating_add(p.packet.duration_ns))
                    .max()?,
            );
        }
        Some(end)
    }

    pub fn push_video(&mut self, packet: &EncodedPacket) {
        self.push(packet, true);
    }
    pub fn push_audio(&mut self, packet: &EncodedPacket) {
        self.push(packet, false);
    }

    fn push(&mut self, packet: &EncodedPacket, video: bool) {
        if !video && packet.track_id as usize >= self.audio.len() {
            return;
        }
        if !video && let Some(first) = self.first_audio_pts.get_mut(packet.track_id as usize) {
            *first = Some(first.map_or(packet.pts.0, |value| value.min(packet.pts.0)));
        }
        // Charge packet metadata as well as its encoded payload to the byte cap.
        let size = packet_bytes(packet);
        if size > self.limit.max_bytes {
            self.packets.clear();
            self.bytes = 0;
            return;
        }
        self.bytes += size;
        self.latest_end = self
            .latest_end
            .max(packet.pts.0.saturating_add(packet.duration_ns));
        self.packets.push_back(Arc::new(ReplayPacket {
            packet: {
                let mut retained = packet.clone();
                if video && retained.keyframe {
                    // Driver keyframe flags alone cannot establish a replay's
                    // independent beginning (notably HEVC CRA/dependent slices).
                    retained.keyframe = crate::independently_decodable_start(
                        self.stream.bitstream_format,
                        &retained.data,
                    );
                }
                retained
            },
            video,
        }));
        self.evict();
    }

    fn evict(&mut self) {
        let cutoff = self
            .latest_end
            .saturating_sub(self.limit.duration_ns.saturating_add(KEYFRAME_LOOKBACK_NS));
        while self.bytes > self.limit.max_bytes
            || self
                .packets
                .front()
                .is_some_and(|entry| entry.packet.pts.0 < cutoff)
        {
            self.pop_front();
        }
        // Retain Opus decoder pre-roll before the earliest usable video keyframe.
        // Earlier video deltas cannot start a replay.
        if let Some(keyframe) = self
            .packets
            .iter()
            .find(|entry| entry.video && entry.packet.keyframe)
            .map(|entry| entry.packet.pts.0)
        {
            self.packets.retain(|entry| {
                !entry.video || entry.packet.keyframe || entry.packet.pts.0 >= keyframe
            });
            self.packets.retain(|entry| {
                entry.video
                    || entry.packet.pts.0.saturating_add(entry.packet.duration_ns)
                        > keyframe.saturating_sub(OPUS_PREROLL_NS)
            });
            self.bytes = self
                .packets
                .iter()
                .map(|entry| packet_bytes(&entry.packet))
                .sum();
        }
    }

    fn pop_front(&mut self) {
        if let Some(old) = self.packets.pop_front() {
            self.bytes -= packet_bytes(&old.packet);
        }
    }

    pub fn save(&self, path: &Path, duration_ns: u64) -> Result<RecordingArtifact, RecorderError> {
        self.save_internal(path, duration_ns, None, None)
    }

    /// New exports carry the same atomic Tracks/checkpoint/integrity manifest
    /// used by recordings. Validation runs before creating any output file.
    pub fn save_session(
        &self,
        path: &Path,
        duration_ns: u64,
    ) -> Result<RecordingArtifact, RecorderError> {
        let mut manifest_path = path.as_os_str().to_os_string();
        manifest_path.push(".freym-session");
        self.save_internal(path, duration_ns, Some(PathBuf::from(manifest_path)), None)
    }

    /// Disk-worker snapshot of the opt-in recording events. Exact session clocks
    /// remain in the framed sidecar; the replay manifest supplies its own origin.
    pub fn save_session_with_metadata(
        &self,
        path: &Path,
        duration_ns: u64,
        metadata: Option<&Path>,
    ) -> Result<RecordingArtifact, RecorderError> {
        let mut manifest_path = path.as_os_str().to_os_string();
        manifest_path.push(".freym-session");
        self.save_internal_with_metadata(
            path,
            duration_ns,
            Some(manifest_path.into()),
            None,
            metadata,
            None,
        )
    }
    pub fn save_session_with_rolling_metadata(
        &self,
        path: &Path,
        duration_ns: u64,
        metadata: &frame_metadata::ReplayMetadataSnapshot,
    ) -> Result<RecordingArtifact, RecorderError> {
        let mut manifest = path.as_os_str().to_owned();
        manifest.push(".freym-session");
        self.save_internal_with_metadata(
            path,
            duration_ns,
            Some(manifest.into()),
            None,
            None,
            Some(metadata),
        )
    }

    pub(super) fn save_internal(
        &self,
        path: &Path,
        duration_ns: u64,
        manifest_path: Option<PathBuf>,
        existing_index: Option<&mut crate::RecordingSession>,
    ) -> Result<RecordingArtifact, RecorderError> {
        self.save_internal_with_metadata(
            path,
            duration_ns,
            manifest_path,
            existing_index,
            None,
            None,
        )
    }
    fn save_internal_with_metadata(
        &self,
        path: &Path,
        duration_ns: u64,
        manifest_path: Option<PathBuf>,
        existing_index: Option<&mut crate::RecordingSession>,
        metadata: Option<&Path>,
        rolling: Option<&frame_metadata::ReplayMetadataSnapshot>,
    ) -> Result<RecordingArtifact, RecorderError> {
        let window = if existing_index.is_some() {
            // Background archive seals may span a delayed checkpoint. The same
            // bounded RAP lookback covers this small scheduling margin.
            self.limit.duration_ns.saturating_add(KEYFRAME_LOOKBACK_NS)
        } else {
            self.limit.duration_ns
        };
        if duration_ns == 0 || duration_ns > window {
            return Err(RecorderError::invalid(
                "Requested replay duration exceeds its configured buffer.",
            ));
        }
        let video_end = self
            .packets
            .iter()
            .filter(|e| e.video)
            .map(|e| e.packet.pts.0.saturating_add(e.packet.duration_ns))
            .max();
        let mut end = video_end
            .ok_or_else(|| RecorderError::invalid("Replay has no retained video keyframe."))?;
        for track in 0..self.audio.len() {
            let audio_end = self
                .packets
                .iter()
                .filter(|e| !e.video && e.packet.track_id as usize == track)
                .map(|e| e.packet.pts.0.saturating_add(e.packet.duration_ns))
                .max()
                .ok_or_else(|| RecorderError::invalid("Replay is missing an audio track."))?;
            end = end.min(audio_end);
        }
        let requested_start = end.checked_sub(duration_ns).ok_or_else(|| {
            RecorderError::invalid("Replay does not yet contain the requested duration.")
        })?;
        let keyframe = self
            .packets
            .iter()
            .filter(|e| e.video && e.packet.keyframe && e.packet.pts.0 <= requested_start)
            .map(|e| e.packet.pts.0)
            .max()
            .ok_or_else(|| {
                RecorderError::invalid(
                    "Replay lacks a retained keyframe before the requested start.",
                )
            })?;
        let mut audio_first = Vec::new();
        let mut replay_tracks = Vec::new();
        for (track, template) in self.audio.iter().enumerate() {
            let preroll_start = keyframe.saturating_sub(OPUS_PREROLL_NS);
            let first = self
                .packets
                .iter()
                .filter(|e| {
                    !e.video
                        && e.packet.track_id as usize == track
                        && e.packet.pts.0 <= preroll_start
                        && e.packet.pts.0.saturating_add(e.packet.duration_ns) > preroll_start
                })
                .map(|e| e.packet.pts.0)
                .max()
                .ok_or_else(|| {
                    RecorderError::invalid(
                        "Replay lacks Opus decoder pre-roll before its starting keyframe.",
                    )
                })?;
            let delta = keyframe - first;
            let mut samples = (u128::from(delta) * 48_000).div_ceil(1_000_000_000);
            if self.first_audio_pts[track] == Some(first) {
                samples += u128::from(template.pre_skip());
            }
            replay_tracks.push(
                template
                    .with_pre_skip(u16::try_from(samples).map_err(|_| {
                        RecorderError::invalid("Replay Opus start trim overflows.")
                    })?)?,
            );
            audio_first.push(first);
        }
        let origin = audio_first.iter().copied().min().unwrap_or(keyframe);
        let mut selected: Vec<&ReplayPacket> = self
            .packets
            .iter()
            .map(Arc::as_ref)
            .filter(|e| {
                e.packet.pts.0 < end
                    && if e.video {
                        e.packet.pts.0 >= keyframe
                    } else {
                        audio_first
                            .get(e.packet.track_id as usize)
                            .is_some_and(|first| e.packet.pts.0 >= *first)
                    }
            })
            .collect();
        selected.sort_by_key(|e| {
            (
                e.packet.pts.0,
                if e.video { 0 } else { 1 },
                e.packet.track_id,
            )
        });
        if selected
            .iter()
            .find(|e| e.video)
            .is_none_or(|e| !e.packet.keyframe)
        {
            return Err(RecorderError::invalid(
                "Replay begins without a video keyframe.",
            ));
        }
        for (track, &first_pts) in audio_first.iter().enumerate() {
            let first = selected
                .iter()
                .find(|e| !e.video && e.packet.track_id as usize == track);
            let last = selected
                .iter()
                .rev()
                .find(|e| !e.video && e.packet.track_id as usize == track);
            if first.is_none_or(|e| e.packet.pts.0 != first_pts)
                || last.is_none_or(|e| e.packet.pts.0.saturating_add(e.packet.duration_ns) < end)
            {
                return Err(RecorderError::invalid(
                    "Replay audio does not cover the selected video range.",
                ));
            }
            let mut expected = first_pts;
            for entry in selected
                .iter()
                .filter(|e| !e.video && e.packet.track_id as usize == track)
            {
                if entry.packet.pts.0 != expected {
                    return Err(RecorderError::invalid(
                        "Replay audio has a missing packet in its selected range.",
                    ));
                }
                expected = expected
                    .checked_add(entry.packet.duration_ns)
                    .ok_or_else(|| RecorderError::invalid("Replay audio timestamp overflowed."))?;
            }
        }
        if let Some(index) = existing_index {
            return self.write_selected(path, selected, replay_tracks, origin, end, Some(index));
        }
        let mut index = manifest_path
            .as_deref()
            .map(crate::RecordingSession::create)
            .transpose()?;
        self.write_selected(path, selected, replay_tracks, origin, end, index.as_mut())
            .and_then(|artifact| {
                if let Some(index) = index.as_mut() {
                    if let Some(source) = metadata {
                        frame_metadata::recover_sidecar(source, &index.metadata_path())
                            .map_err(|e| RecorderError::invalid(format!(
                                "Replay media saved at {}; event snapshot failed: {e}. Preserve the clip and its session manifest.",
                                path.display()
                            )))?;
                        index.attach_metadata()?;
                    }
                    if let Some(events)=rolling {
                        events.write_range(&index.metadata_path(),origin,end).map_err(|e|RecorderError::invalid(format!("Replay media saved at {}; event snapshot failed: {e}. Preserve the clip and manifest.",path.display())))?;
                        index.attach_metadata()?;
                    }
                    index.finish()?;
                }
                Ok(artifact)
            })
    }

    fn write_selected(
        &self,
        path: &Path,
        selected: Vec<&ReplayPacket>,
        replay_tracks: Vec<AvAudioTrack>,
        origin: u64,
        end: u64,
        session: Option<&mut crate::RecordingSession>,
    ) -> Result<RecordingArtifact, RecorderError> {
        if self.audio.is_empty() {
            let mut writer = if let Some(index) = session {
                index.create_video_segment(&self.stream, MediaTimestamp(origin))?
            } else {
                MkvVideoWriter::create(path, &self.stream, MediaTimestamp(origin))?
            };
            for entry in selected {
                writer.write(entry.packet.clone())?;
            }
            return writer.finalize();
        }
        let mut writer = if let Some(index) = session {
            index.create_av_segment(&self.stream, &replay_tracks, MediaTimestamp(origin), false)?
        } else {
            MkvAvWriter::create(path, &self.stream, &replay_tracks, MediaTimestamp(origin))?
        };
        let mut last_audio_end = vec![0u64; self.audio.len()];
        for entry in selected {
            let mut packet = entry.packet.clone();
            if entry.video {
                packet.duration_ns = packet.duration_ns.min(end - packet.pts.0);
                writer.write_video(packet)?;
            } else {
                let index = packet.track_id as usize;
                last_audio_end[index] = packet.pts.0 + packet.duration_ns;
                writer.write_audio(packet)?;
            }
        }
        for (index, audio_end) in last_audio_end.into_iter().enumerate() {
            let trim_ns = audio_end.saturating_sub(end);
            let trim_samples = (trim_ns / 1_000_000_000 * 48_000)
                + ((trim_ns % 1_000_000_000) * 48_000 / 1_000_000_000);
            let trim = u16::try_from(trim_samples)
                .map_err(|_| RecorderError::invalid("Replay Opus end trim exceeds one packet."))?;
            writer.finish_audio(index, trim)?;
        }
        writer.finalize()
    }
}

fn packet_bytes(packet: &EncodedPacket) -> usize {
    packet
        .data
        .len()
        .saturating_add(std::mem::size_of::<ReplayPacket>())
        .saturating_add(3 * std::mem::size_of::<usize>()) // Arc counts + deque slot
}
