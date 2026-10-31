// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Streaming through mkwriter's EBML/block primitives. Its high-level muxer omits
//! ReferenceBlock for non-keyframe BlockGroups and builds a full cue tree at finalize;
//! this adapter keeps those two policies explicit and bounded.
//!
//! The adapter is track-aware but otherwise unchanged by the number of tracks: one
//! video channel and the audio channels share the same cluster stream, so blocks keep
//! their global delivery order and a cluster rolls over exactly as before. Only video
//! keyframes become cue points; audio packets are not seek index entries.

use super::DurableCheckpoint;
use super::output::SinkAdapter;
use mkwriter::block::{Lacing, SimpleBlock};
use mkwriter::element::{Node, Value};
use mkwriter::ids::ElementId;
use mkwriter::sink::{MasterScope, Sink};
use mkwriter::{EbmlWriter, Frame, Info, Tracks, ebml, ids};
use std::io::Write;
use std::time::{Duration, Instant};

/// Check once the first video block exists, then at most once per five seconds
/// of writing. A quiet/static source does not trigger background disk wakeups.
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(5);

type Result<T> = mkwriter::Result<T>;

/// Writes an element whose value octets are used exactly as given, so a fixed-width
/// value keeps its size and can be patched in place later.
trait FixedElement {
    fn write_element_fixed(&mut self, id: ElementId, value: &[u8]) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mkv::test_support::FailingTarget;
    use mkwriter::{TrackEntry, VideoSettings};

    fn backend() -> (Backend, FailingTarget) {
        let target = FailingTarget::new(usize::MAX);
        let tracks = Tracks {
            entries: vec![
                TrackEntry::new(1, 1, ids::track_type::VIDEO, "V_MPEG4/ISO/AVC")
                    .video(VideoSettings::new(64, 48)),
            ],
        };
        let backend = Backend::new(
            SinkAdapter(Box::new(target.clone())),
            &crate::mkv::container_info(),
            &tracks,
        )
        .unwrap();
        (backend, target)
    }

    fn frame(timestamp_ns: u64, keyframe: bool) -> Frame<'static> {
        Frame {
            track: 1,
            timestamp_ns,
            duration_ns: Some(1_000_000),
            data: &[0, 0, 0, 2, 0x65, 0x80],
            keyframe,
            ..Frame::default()
        }
    }

    #[test]
    fn checkpoints_close_clusters_and_use_a_wall_clock_cadence() {
        let (mut backend, target) = backend();
        backend.write_frame(frame(0, true), false).unwrap();
        let first = backend.last_checkpoint().unwrap();
        assert_eq!(first.sequence, 1);
        assert_eq!(first.position_bytes, target.bytes().len() as u64);
        assert!(backend.cluster.is_none(), "the covered cluster is closed");
        let at = backend.checkpoint_time.unwrap();
        for index in 1..=100 {
            backend
                .write_frame(frame(index * 1_000_000, false), false)
                .unwrap();
        }
        backend
            .checkpoint_if_due(at + Duration::from_secs(4))
            .unwrap();
        assert_eq!(
            target.checkpoints(),
            1,
            "no per-frame sync or timestamp-triggered sync"
        );
        backend.checkpoint_if_due(at + CHECKPOINT_INTERVAL).unwrap();
        let second = backend.last_checkpoint().unwrap();
        assert_eq!(target.checkpoints(), 2);
        assert_eq!(second.sequence, 2);
        assert!(second.position_bytes > first.position_bytes);
        assert_eq!(second.container_end_ns, 101_000_000);
        assert!(backend.cluster.is_none());
    }

    #[test]
    fn a_failed_sync_never_advances_the_successful_checkpoint() {
        let (mut backend, target) = backend();
        backend.write_frame(frame(0, true), false).unwrap();
        let before = backend.last_checkpoint().unwrap();
        backend.write_frame(frame(1_000_000, false), false).unwrap();
        target.fail_checkpoint();
        assert!(backend.checkpoint().is_err());
        assert_eq!(backend.last_checkpoint(), Some(before));
        assert_eq!(target.checkpoints(), 1);
    }
}

impl FixedElement for EbmlWriter<SinkAdapter> {
    fn write_element_fixed(&mut self, id: ElementId, value: &[u8]) -> Result<()> {
        let mut header = Vec::with_capacity(12);
        ebml::write_id(&mut header, id.as_u32())?;
        ebml::write_vint(&mut header, value.len() as u64)?;
        self.get_mut().write_all(&header)?;
        self.get_mut().write_all(value)?;
        Ok(())
    }
}

/// One indexed random access point: container time, absolute cluster position and
/// the block's position inside that cluster.
#[derive(Debug, Clone, Copy)]
struct Cue {
    time: u64,
    cluster: u64,
    relative: u64,
    track: u64,
}

pub(super) struct Backend {
    out: EbmlWriter<SinkAdapter>,
    segment: MasterScope,
    cluster: Option<(MasterScope, u64, u64)>,
    seek_start: u64,
    seek_bytes: usize,
    info_position: u64,
    tracks_position: u64,
    duration_offset: u64,
    end_units: u64,
    cues: Vec<Cue>,
    payload: Vec<u8>,
    longest_payload: usize,
    scale: u64,
    last_padding_offset: Option<u64>,
    /// File offset of the `BlockDuration` value of the most recent video block whose
    /// duration is still a live placeholder. `None` means no video block is pending.
    last_block_duration_offset: Option<u64>,
    has_video: bool,
    checkpoint_time: Option<Instant>,
    last_checkpoint: Option<DurableCheckpoint>,
    checkpoint_pending: bool,
}

impl Backend {
    pub(super) fn new(target: SinkAdapter, info: &Info, tracks: &Tracks) -> Result<Self> {
        let mut out = EbmlWriter::new(target);
        mkwriter::write_ebml_header(&mut out, "matroska", 4, 2)?;
        let segment = out.begin_streamed_master(ids::SEGMENT)?;
        let seek_start = out.position();
        out.write_void(256)?;
        let seek_bytes = (out.position() - seek_start) as usize;
        let info_position = out.position() - segment.body_start();
        let info_scope = out.begin_streamed_master(ids::INFO)?;
        out.write_uint(ids::TIMESTAMP_SCALE, info.timestamp_scale)?;
        out.write_utf8(ids::MUXING_APP, &info.muxing_app)?;
        out.write_utf8(ids::WRITING_APP, &info.writing_app)?;
        out.write_float(ids::DURATION, 1.0)?;
        let duration_offset = out.position() - 8;
        out.end_streamed_master(info_scope)?;
        let tracks_position = out.position() - segment.body_start();
        let mut node = tracks.node();
        // A stems-only recording still has one deterministic playback track.
        // Prefer Main whenever present; never merge the independent streams.
        let default_audio = tracks
            .entries
            .iter()
            .position(|e| {
                e.track_type == ids::track_type::AUDIO && e.name.as_deref() == Some("main")
            })
            .or_else(|| {
                tracks
                    .entries
                    .iter()
                    .position(|e| e.track_type == ids::track_type::AUDIO)
            });
        if let Value::Master(entries) = &mut node.value {
            for (index, (entry, node)) in tracks.entries.iter().zip(entries).enumerate() {
                if entry.track_type == ids::track_type::AUDIO
                    && let Value::Master(children) = &mut node.value
                {
                    children.push(Node::uint(
                        ids::FLAG_DEFAULT,
                        u64::from(default_audio == Some(index)),
                    ));
                }
            }
        }
        let mut encoded_tracks = EbmlWriter::new(mkwriter::sink::WriteSink::new(Vec::new()));
        encoded_tracks.write_node(&node, r"\Segment")?;
        out.get_mut()
            .0
            .segment_headers(&encoded_tracks.into_inner().into_inner())?;
        out.write_node(&node, r"\Segment")?;
        Ok(Self {
            out,
            segment,
            cluster: None,
            seek_start,
            seek_bytes,
            info_position,
            tracks_position,
            duration_offset,
            end_units: 0,
            cues: Vec::new(),
            payload: Vec::new(),
            longest_payload: 0,
            scale: info.timestamp_scale,
            last_padding_offset: None,
            last_block_duration_offset: None,
            has_video: false,
            checkpoint_time: None,
            last_checkpoint: None,
            checkpoint_pending: false,
        })
    }

    pub(super) fn retained_bytes(&self) -> u64 {
        (self.cues.capacity() * std::mem::size_of::<Cue>() + self.payload.capacity()) as u64
    }

    pub(super) fn projected_bytes(&self, bytes: usize) -> u64 {
        // The caller caps cues at MAX_KEYFRAMES (a power of two), including vector
        // capacity. Finalization streams one cue at a time, without a second index.
        // `bytes` is this packet's exact encoded block size, so `payload` holds at
        // most the larger of what it already owns and the incoming packet.
        let payload = self.longest_payload.max(bytes).max(self.payload.capacity());
        payload as u64 + super::MAX_KEYFRAMES * std::mem::size_of::<Cue>() as u64
    }

    /// `live_video` marks a live video block whose `BlockDuration` is written as a
    /// fixed eight-octet value at the packet's own duration and patched in place later.
    /// Every other block writes its duration as a minimal-width integer, exactly as
    /// before.
    pub(super) fn write_frame(&mut self, frame: Frame<'_>, live_video: bool) -> Result<()> {
        let units = frame.timestamp_ns / self.scale;
        let needs_cluster = self.cluster.as_ref().is_none_or(|(_, base, _)| {
            i16::try_from(i128::from(units) - i128::from(*base)).is_err()
        });
        if needs_cluster {
            if let Some((scope, _, _)) = self.cluster.take() {
                self.out.end_streamed_master(scope)?;
            }
            let position = self.out.position() - self.segment.body_start();
            let scope = self.out.begin_streamed_master(ids::CLUSTER)?;
            self.out.write_uint(ids::TIMESTAMP, units)?;
            self.cluster = Some((scope, units, position));
        }
        let (cluster, base, position) = self.cluster.as_ref().expect("opened cluster");
        let relative_position = self.out.position() - cluster.body_start();
        // A late packet on another track may precede this cluster's base. The
        // signed Block timestamp preserves its PTS within the i16 window.
        let relative = i16::try_from(i128::from(units) - i128::from(*base))
            .expect("the cluster rolls over before the relative timestamp leaves i16");
        let frames = [frame.data];
        let block = SimpleBlock {
            track: frame.track,
            rel_timestamp: relative,
            keyframe: frame.keyframe,
            invisible: false,
            discardable: false,
            lacing: Lacing::None,
            frames: &frames,
        };
        self.payload.clear();
        self.payload.reserve_exact(frame.data.len() + 16);
        block.encode_as_block(&mut self.payload)?;
        self.longest_payload = self.longest_payload.max(self.payload.capacity());
        let group = self.out.begin_streamed_master(ids::BLOCK_GROUP)?;
        self.out.write_binary(ids::BLOCK, &self.payload)?;
        let duration = frame.duration_ns.expect("validated duration") / self.scale;
        self.last_block_duration_offset = None;
        if live_video {
            // A live writer does not know a video frame's duration when the frame is
            // written: the next real timestamp fixes it. The value keeps a fixed eight
            // octets so the real duration can be patched in place later without
            // retaining one byte of the access unit and without moving any other block.
            // Until then it holds one container unit, the shortest duration the file
            // can express, so the element is present and readable at every moment.
            let placeholder = duration.max(1);
            let mut value = [0u8; 8];
            value[1..].copy_from_slice(&placeholder.to_be_bytes()[1..]);
            self.out.write_element_fixed(ids::BLOCK_DURATION, &value)?;
            // The value is exactly eight octets, so its first octet is eight bytes back.
            self.last_block_duration_offset = Some(self.out.position() - 8);
        } else {
            self.out.write_uint(ids::BLOCK_DURATION, duration)?;
        }
        if frame.track != super::VIDEO_TRACK_NUMBER {
            // An Opus packet is a random access point with no reference frames, so a
            // BlockGroup carries no ReferenceBlock for audio. CodecDelay, SeekPreRoll
            // and this block's DiscardPadding describe the codec's own delay.
        } else if !frame.keyframe {
            // Matroska explicitly permits zero for a dependent frame whose exact
            // reference set is not described by the container (the codec knows it).
            self.out.write_int(ids::REFERENCE_BLOCK, 0)?;
        }
        self.last_padding_offset = None;
        if frame.track != super::VIDEO_TRACK_NUMBER {
            // Fixed-width signed integer: patch only the last block at finish without
            // retaining packets or changing their on-disk delivery order.
            let padding = frame.discard_padding_ns.unwrap_or(0);
            self.out
                .write_binary(ids::DISCARD_PADDING, &padding.to_be_bytes())?;
            self.last_padding_offset = Some(self.out.position() - 8);
        }
        self.out.end_streamed_master(group)?;
        if frame.track == super::VIDEO_TRACK_NUMBER && frame.keyframe {
            self.cues.push(Cue {
                time: units,
                cluster: *position,
                relative: relative_position,
                track: frame.track,
            });
        }
        self.end_units = self.end_units.max(units + duration);
        self.has_video |= frame.track == super::VIDEO_TRACK_NUMBER;
        self.checkpoint_if_due(Instant::now())?;
        Ok(())
    }

    pub(super) fn last_checkpoint(&self) -> Option<DurableCheckpoint> {
        self.last_checkpoint
    }

    fn checkpoint_if_due(&mut self, now: Instant) -> Result<()> {
        self.complete_pending_checkpoint(false)?;
        if !self.checkpoint_pending
            && self.has_video
            && self.checkpoint_time.is_none_or(|previous| {
                now.saturating_duration_since(previous) >= CHECKPOINT_INTERVAL
            })
        {
            self.checkpoint_at(now)?;
        }
        Ok(())
    }

    /// Close the current cluster so every master inside the covered prefix has
    /// its real size. Keep Segment unfinished and start a fresh cluster on the
    /// next packet; duration/padding patch offsets and track timestamps stay valid.
    pub(super) fn checkpoint(&mut self) -> Result<DurableCheckpoint> {
        self.complete_pending_checkpoint(true)?;
        self.checkpoint_at(Instant::now())?;
        self.complete_pending_checkpoint(true)?;
        Ok(self.last_checkpoint.expect("acknowledged checkpoint"))
    }

    fn complete_pending_checkpoint(&mut self, wait: bool) -> Result<()> {
        if self.checkpoint_pending
            && let Some(point) = self.out.get_mut().0.poll_checkpoint(wait)?
        {
            self.last_checkpoint = Some(point);
            self.checkpoint_pending = false;
        }
        Ok(())
    }

    fn checkpoint_at(&mut self, now: Instant) -> Result<()> {
        if !self.has_video {
            return Err(mkwriter::Error::Io(std::io::Error::other(
                "no video block exists to checkpoint",
            )));
        }
        if let Some((scope, _, _)) = self.cluster.take() {
            self.out.end_streamed_master(scope)?;
        }
        let checkpoint = DurableCheckpoint {
            sequence: self.last_checkpoint.map_or(1, |point| point.sequence + 1),
            position_bytes: self.out.position(),
            container_end_ns: self.end_units * self.scale,
        };
        match self.out.get_mut().0.request_checkpoint(checkpoint)? {
            Some(confirmed) => self.last_checkpoint = Some(confirmed),
            None => self.checkpoint_pending = true,
        }
        // Only the request cadence advances here. The public durable point waits
        // for BOTH file sync and manifest publication, and any error is sticky.
        self.checkpoint_time = Some(now);
        Ok(())
    }

    /// `cue_tracks` lists the container track numbers that may own a cue point; a
    /// video keyframe is indexed only when its track is in that list.
    pub(super) fn last_padding_offset(&self) -> Option<u64> {
        self.last_padding_offset
    }

    /// File offset of the pending video `BlockDuration` value, if the last video block
    /// was written in live mode.
    pub(super) fn last_block_duration_offset(&self) -> Option<u64> {
        self.last_block_duration_offset
    }

    /// Patches one video block's fixed-width `BlockDuration` in place. No byte moves and no
    /// delivery order changes; only the eight value octets at that offset are rewritten, so
    /// a block written earlier can be corrected after later blocks were appended.
    pub(super) fn patch_block_duration(&mut self, offset: u64, units: u64) -> Result<()> {
        self.out
            .get_mut()
            .patch(offset, &units.to_be_bytes())
            .map_err(mkwriter::Error::Io)
    }

    /// Clears the pending marker once the block's real duration is written.
    pub(super) fn clear_block_duration(&mut self) {
        self.last_block_duration_offset = None;
    }

    pub(super) fn patch_padding(&mut self, offset: u64, padding_ns: i64) -> Result<()> {
        self.out
            .get_mut()
            .patch(offset, &padding_ns.to_be_bytes())?;
        Ok(())
    }

    pub(super) fn finalize(
        mut self,
        cue_tracks: &[u64],
        duration_units: Option<u64>,
    ) -> Result<SinkAdapter> {
        self.complete_pending_checkpoint(true)?;
        if let Some((scope, _, _)) = self.cluster.take() {
            self.out.end_streamed_master(scope)?;
        }
        let cues_position = self.out.position() - self.segment.body_start();
        let cues_scope = self.out.begin_streamed_master(ids::CUES)?;
        for cue in self
            .cues
            .iter()
            .filter(|cue| cue_tracks.contains(&cue.track))
        {
            self.out.write_master(ids::CUE_POINT, |writer| {
                writer.write_uint(ids::CUE_TIME, cue.time)?;
                writer.write_master(ids::CUE_TRACK_POSITIONS, |track| {
                    track.write_uint(ids::CUE_TRACK, cue.track)?;
                    track.write_uint(ids::CUE_CLUSTER_POSITION, cue.cluster)?;
                    track.write_uint(ids::CUE_RELATIVE_POSITION, cue.relative)
                })
            })?;
        }
        self.out.end_streamed_master(cues_scope)?;
        let mut seek = mkwriter::seekhead::SeekHead::new();
        seek.add(ids::INFO, self.info_position);
        seek.add(ids::TRACKS, self.tracks_position);
        seek.add(ids::CUES, cues_position);
        let mut patch = seek.to_bytes()?;
        let remainder = self.seek_bytes.checked_sub(patch.len()).ok_or_else(|| {
            mkwriter::Error::Io(std::io::Error::other("seek header reservation exhausted"))
        })?;
        if remainder >= 2 {
            // Use the library's VINT encoder to fill the reserved bytes exactly.
            let width = (1..=8)
                .find(|width| {
                    remainder > *width
                        && ((remainder - 1 - *width) as u64) < (1u64 << (7 * width)) - 1
                })
                .expect("small fixed seek reservation");
            ebml::write_id(&mut patch, ids::VOID.as_u32())?;
            ebml::write_vint_with_len(&mut patch, (remainder - 1 - width) as u64, width)?;
            patch.resize(self.seek_bytes, 0);
        } else if remainder != 0 {
            return Err(mkwriter::Error::Io(std::io::Error::other(
                "invalid seek padding",
            )));
        }
        self.out.get_mut().patch(self.seek_start, &patch)?;
        self.out.get_mut().patch(
            self.duration_offset,
            &(duration_units.unwrap_or(self.end_units) as f64).to_be_bytes(),
        )?;
        self.out.end_streamed_master(self.segment)?;
        self.out.get_mut().flush()?;
        self.out.get_mut().0.segment_finalization(
            duration_units.unwrap_or(self.end_units) * self.scale,
            self.cues
                .iter()
                .filter(|cue| cue_tracks.contains(&cue.track))
                .count() as u64,
        );
        Ok(self.out.into_inner())
    }
}
