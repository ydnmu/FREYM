// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! One H.264 or HEVC video track plus the audio plan's Opus tracks in one Matroska file.
//!
//! [`MkvAvWriter`] shares the EBML backend, the container header, the cluster stream
//! and the cue index with [`MkvVideoWriter`](super::MkvVideoWriter); nothing about the
//! video path changes when audio is added. Container track `1` is the video track and
//! audio plan index `i` is container track `i + 2`.
//!
//! Two write entries keep the two packet identities apart: a video [`EncodedPacket`]
//! goes to [`MkvAvWriter::write_video`] and an audio one to
//! [`MkvAvWriter::write_audio`]. `EncodedPacket` is the existing encoder contract and
//! is not redeclared here.
//!
//! Every packet keeps the caller's shared origin and the existing 1 µs container
//! scale. Packets are written in delivery order, including a late packet on another
//! track with an earlier PTS. Timestamps must increase strictly within each track.
//! A signed Block offset represents a late packet in the current cluster when it fits;
//! otherwise the backend opens a new cluster at that packet's time. Audio is written
//! immediately with a reserved
//! zero DiscardPadding field. Finishing patches only the last field; no audio
//! payload or reordering queue is retained.
//!
//! # Live mode
//!
//! [`MkvAvWriter::create_live`] is the recording mode of a live screen + audio
//! session. A live video packet's duration is not known when the packet is written —
//! only the next real timestamp fixes it — so the video `BlockDuration` is written as
//! a fixed eight-octet value and patched **in place** when the next video packet
//! arrives, and the very last one when the caller closes the video stream. The writer
//! keeps exactly one offset and one PTS for that, never a payload; it never changes a
//! packet's PTS, delivery order or block size, and the same file-writing API is used
//! in both modes.
//!
//! # Opus timing
//!
//! `pre_skip` is the encoder's own lookahead in 48 kHz samples per channel. It is
//! written into the track's `OpusHead` CodecPrivate and as the track's `CodecDelay`;
//! the writer applies no second delay of its own. `SeekPreRoll` is the 80 ms Matroska
//! requires for Opus. Only the last packet of a track carries `DiscardPadding`, taken
//! from the encoder's flush. A track's real end is therefore
//! `last PTS + last duration - codec delay - discard padding`, and an artifact is never produced
//! before every declared audio track was finished with at least one packet.

use crate::mkv::backend::Backend;
use crate::mkv::errors::{
    backend_error, backend_limit_error, output_path_error, request_error, storage_error,
    writer_error,
};
use crate::mkv::output::{OutputFile, OutputTarget, SinkAdapter};
use crate::mkv::{
    DurableCheckpoint, FINALIZE_SCRATCH_BYTES, MAX_AUDIO_TRACKS, MAX_KEYFRAMES, MAX_PACKET_BYTES,
    MAX_RETAINED_BYTES, StreamPlan, TIMESTAMP_SCALE_NS, VIDEO_TRACK_NUMBER, container_info,
    plan_stream, to_units, video_access_unit, video_access_unit_size, video_settings,
};
use frame_audio::{TrackKind, TrackPlan};
use frame_clock::MediaTimestamp;
use frame_contracts::{ErrorCode, RecorderError, RecordingArtifact, RecoveryStatus};
use frame_encode::{BitstreamFormat, EncodedPacket, EncodedStreamInfo};
use mkwriter::{AudioSettings, Frame, TrackEntry, Tracks, ids::track_type};
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

/// The only sample rate this writer declares. No resampling happens here.
const OPUS_SAMPLE_RATE: u64 = 48_000;
/// Stereo default used by legacy callers and header fixtures.
#[cfg(test)]
const OPUS_CHANNELS: u16 = 2;
/// Matroska's `SeekPreRoll` for Opus, in nanoseconds.
const OPUS_SEEK_PRE_ROLL_NS: u64 = 80_000_000;
/// `OpusHead` payload size for version 1 with no channel mapping table.
const OPUS_HEAD_BYTES: usize = 19;
/// Codec identifier of an Opus track in Matroska.
const OPUS_CODEC_ID: &str = "A_OPUS";
/// Codec identifier of the H.264 track in Matroska, as the video writer declares it.
/// Every Opus packet of this writer lasts exactly one 20 ms frame.
const OPUS_PACKET_NS: u64 = 20_000_000;
/// Trailing padding must be shorter than one 20 ms packet.
const MAX_DISCARD_PADDING_SAMPLES: u16 = 959;
const MAX_SOURCE_ID_BYTES: usize = 64 * 1024;

const AUDIO_ACTION: &str = "Send each Opus track's packets in strictly increasing container time and finish every declared track with the encoder's own discard padding.";
const PLAN_ACTION: &str =
    "Build the audio plan before the container and declare exactly the tracks it contains.";

/// One audio track the container must hold, in `TrackPlan` order.
///
/// Source identities remain internal. Container labels are `main`, `clean-1`,
/// `clean-2`, in plan order. Pre-skip comes from the real encoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvAudioTrack {
    channels: u16,
    kind: TrackKind,
    pre_skip: u16,
}

impl AvAudioTrack {
    pub fn from_plan(plan: &TrackPlan, pre_skip: u16) -> Result<Self, RecorderError> {
        if pre_skip >= 960
            || matches!(&plan.kind, TrackKind::Clean(id)
            if id.0.is_empty() || id.0.len() > MAX_SOURCE_ID_BYTES)
        {
            return Err(request_error(
                "Invalid Opus pre-skip or bounded source identifier.",
                RecoveryStatus::NoRecording,
            ));
        }
        Ok(Self {
            channels: 2,
            kind: plan.kind.clone(),
            pre_skip,
        })
    }

    pub fn with_channels(mut self, channels: std::num::NonZeroU16) -> Result<Self, RecorderError> {
        if !matches!(channels.get(), 1 | 2) {
            return Err(RecorderError::unsupported("Opus channel layout"));
        }
        self.channels = channels.get();
        Ok(self)
    }
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// The plan kind this track was built from, kept for evidence and callers.
    pub fn kind(&self) -> &TrackKind {
        &self.kind
    }

    /// The real Opus pre-skip in 48 kHz samples per channel.
    pub fn pre_skip(&self) -> u16 {
        self.pre_skip
    }

    /// Replay carries decoder pre-roll before its first audible sample. Its
    /// CodecDelay discards that pre-roll and the partial packet at the keyframe.
    pub fn with_pre_skip(&self, pre_skip: u16) -> Result<Self, RecorderError> {
        if pre_skip >= 5 * 960 {
            return Err(request_error(
                "Replay Opus start trim exceeds five packets.",
                RecoveryStatus::NoRecording,
            ));
        }
        Ok(Self {
            channels: self.channels,
            kind: self.kind.clone(),
            pre_skip,
        })
    }
}

/// Counters and bounds of one audio/video writer, useful for diagnostics and evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MkvAvWriterStats {
    pub video_packets: u64,
    pub video_keyframes: u64,
    pub audio_tracks: u32,
    pub audio_packets: u64,
    pub audio_finished_tracks: u32,
    pub raw_payload_bytes: u64,
    pub first_video_pts_ns: Option<u64>,
    /// Container time of the furthest written packet end, including audio padding.
    pub last_end_ns: u64,
    /// The real media end after every finished track's trim; the artifact duration.
    pub trimmed_end_ns: u64,
    pub retained_bytes_estimate: u64,
    /// True for a live writer whose video `BlockDuration` values are patched in place.
    pub live_block_durations: bool,
    /// Video blocks whose duration was already patched with a measured interval.
    pub video_duration_patches: u64,
    /// True while the last video block still waits for its real duration.
    pub video_duration_pending: bool,
    /// Flush/sync success at a complete-element prefix; final live duration and
    /// Opus trim may still be unknown.
    pub last_checkpoint: Option<DurableCheckpoint>,
}

#[derive(Debug)]
struct VideoTrack {
    packets: u64,
    keyframes: u64,
    last_pts_units: Option<u64>,
    end_units: u64,
    first_pts_units: Option<u64>,
    longest_packet_bytes: u64,
    packet_track_id: Option<u32>,
}

impl VideoTrack {
    fn new() -> Self {
        Self {
            packets: 0,
            keyframes: 0,
            last_pts_units: None,
            end_units: 0,
            first_pts_units: None,
            longest_packet_bytes: 0,
            packet_track_id: None,
        }
    }
}

#[derive(Debug)]
struct AudioTrack {
    /// Container track number: audio plan index plus two, with video at 1.
    number: u64,
    name: String,
    kind: TrackKind,
    /// Kept as evidence of the exact pre-skip this container declares.
    pre_skip: u16,
    /// Packets accepted through `write_audio`, all immediately written.
    accepted: u64,
    packets: u64,
    /// Container end of the most recent packet, before any trim.
    last_end_units: u64,
    /// Real end after `discard_padding_units`; the trimmed timeline.
    trimmed_end_units: u64,
    last_pts_units: Option<u64>,
    discard_padding_units: u64,
    discard_padding_samples: u16,
    finished: bool,
    longest_packet_bytes: u64,
    last_padding_offset: Option<u64>,
}

/// A validated audio track, before the container exists.
#[derive(Debug)]
struct PlannedAudioTrack {
    channels: u16,
    number: u64,
    name: String,
    kind: TrackKind,
    pre_skip: u16,
}

/// Which write entry a packet came through, resolved before any state changes.
#[derive(Debug, Clone, Copy)]
enum Slot {
    Video,
    Audio(usize),
}

/// How a video packet's duration reaches the container.
///
/// `Complete` is the existing behaviour: the caller already knows the duration, so the
/// value is written straight through. `Live` is a live recording: the duration is
/// unknown at write time, so a fixed-width placeholder is patched in place as soon as
/// the next real timestamp is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteMode {
    Complete,
    Live,
}

/// The one video block whose duration the live writer still owes.
#[derive(Debug, Clone, Copy)]
struct PendingVideoDuration {
    offset: u64,
    pts_ns: u64,
}

/// One validated frame, ready for the frame backend.
#[derive(Debug)]
struct PreparedFrame {
    track: u64,
    timestamp_ns: u64,
    duration_ns: u64,
    /// Container end of this packet, already checked to be representable.
    end_units: u64,
    keyframe: bool,
    discard_padding_ns: Option<i64>,
}

pub struct MkvAvWriter {
    path: PathBuf,
    muxer: Option<Backend>,
    origin_ns: u64,
    video: VideoTrack,
    audio: Vec<AudioTrack>,
    failure: Option<RecorderError>,
    raw_bytes: u64,
    /// Video conversion scratch, reused for every video packet.
    scratch: Vec<u8>,
    done: bool,
    mode: WriteMode,
    /// The one video block whose live duration is still pending, with its PTS. Never a
    /// payload: only an in-place patch offset and the timestamp it was written with.
    pending_video: Option<PendingVideoDuration>,
    video_duration_patches: u64,
    bitstream_format: BitstreamFormat,
    av1_sequence: Option<Vec<u8>>,
}

impl std::fmt::Debug for MkvAvWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MkvAvWriter")
            .field("path", &self.path)
            .field("origin_ns", &self.origin_ns)
            .field("audio_tracks", &self.audio.len())
            .field("video_packets", &self.video.packets)
            .field("failed", &self.failed())
            .finish_non_exhaustive()
    }
}

impl MkvAvWriter {
    /// Creates a new file holding exactly this video stream and these audio tracks.
    /// An existing path is never overwritten.
    ///
    /// This is the completing mode: every packet already carries its own duration.
    pub fn create(
        path: &Path,
        stream: &EncodedStreamInfo,
        tracks: &[AvAudioTrack],
        origin: MediaTimestamp,
    ) -> Result<Self, RecorderError> {
        Self::open(path, stream, tracks, origin, WriteMode::Complete)
    }

    /// Creates a new file for one **live** recording of this video stream and these
    /// audio tracks. An existing path is never overwritten.
    ///
    /// The video `BlockDuration` of each live packet is a fixed-width placeholder that
    /// [`MkvAvWriter::write_video`] patches in place with the real interval once the
    /// next packet's timestamp is known; [`MkvAvWriter::finish_video`] closes the last
    /// one. Nothing else about the file differs from [`MkvAvWriter::create`].
    pub fn create_live(
        path: &Path,
        stream: &EncodedStreamInfo,
        tracks: &[AvAudioTrack],
        origin: MediaTimestamp,
    ) -> Result<Self, RecorderError> {
        Self::open(path, stream, tracks, origin, WriteMode::Live)
    }

    /// Both constructors share one body, so a live file differs from a completing one
    /// only in how a video packet's duration reaches the container.
    fn open(
        path: &Path,
        stream: &EncodedStreamInfo,
        tracks: &[AvAudioTrack],
        origin: MediaTimestamp,
        mode: WriteMode,
    ) -> Result<Self, RecorderError> {
        let plan = plan_stream(stream)?;
        let tracks = prepare_audio_tracks(tracks)?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| output_path_error(path, &error))?;
        let target = Box::new(OutputFile::recording(file, path).map_err(|error| {
            storage_error(&error, "The output file could not be prepared for writing.")
        })?);
        Self::with_target(path.to_path_buf(), target, plan, tracks, origin, mode)
    }

    pub(crate) fn create_segment(
        path: &Path,
        stream: &EncodedStreamInfo,
        tracks: &[AvAudioTrack],
        origin: MediaTimestamp,
        live: bool,
        lease: crate::session::SegmentLease,
    ) -> Result<Self, RecorderError> {
        let plan = plan_stream(stream)?;
        let tracks = prepare_audio_tracks(tracks)?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| output_path_error(path, &error))?;
        let target = Box::new(
            OutputFile::segment(file, path, lease)
                .map_err(|error| storage_error(&error, "The segment could not be prepared."))?,
        );
        Self::with_target(
            path.to_path_buf(),
            target,
            plan,
            tracks,
            origin,
            if live {
                WriteMode::Live
            } else {
                WriteMode::Complete
            },
        )
    }

    fn with_target(
        path: PathBuf,
        target: Box<dyn OutputTarget>,
        plan: StreamPlan,
        audio: Vec<PlannedAudioTrack>,
        origin: MediaTimestamp,
        mode: WriteMode,
    ) -> Result<Self, RecorderError> {
        let info = container_info();
        let mut entries = Vec::with_capacity(audio.len() + 1);
        let settings = video_settings(&plan);
        entries.push(
            TrackEntry::new(
                VIDEO_TRACK_NUMBER,
                video_track_uid(),
                track_type::VIDEO,
                plan.codec_id,
            )
            .with_codec_private(plan.codec_private)
            .video(settings),
        );
        for track in &audio {
            entries.push(
                TrackEntry::new(
                    track.number,
                    track_uid(track.number),
                    track_type::AUDIO,
                    OPUS_CODEC_ID,
                )
                .with_codec_private(opus_head_channels(track.pre_skip, track.channels))
                .with_opus_delays(
                    samples_to_ns(u64::from(track.pre_skip)),
                    OPUS_SEEK_PRE_ROLL_NS,
                )
                // Backend selects Main, or the first stem in a separate-only plan.
                .with_name(track.name.clone())
                .audio(AudioSettings {
                    sampling_frequency: OPUS_SAMPLE_RATE as f64,
                    channels: u64::from(track.channels),
                    bit_depth: None,
                }),
            );
        }
        let tracks = Tracks { entries };
        let muxer = Backend::new(SinkAdapter(target), &info, &tracks)
            .map_err(|error| backend_error(error, "The container header could not be written."))?;

        let audio: Vec<AudioTrack> = audio
            .into_iter()
            .map(|planned| AudioTrack {
                number: planned.number,
                name: planned.name,
                kind: planned.kind,
                pre_skip: planned.pre_skip,
                accepted: 0,
                packets: 0,
                last_end_units: 0,
                trimmed_end_units: 0,
                last_pts_units: None,
                discard_padding_units: 0,
                discard_padding_samples: 0,
                finished: false,
                longest_packet_bytes: 0,
                last_padding_offset: None,
            })
            .collect();
        Ok(Self {
            path,
            muxer: Some(muxer),
            origin_ns: origin.0,
            video: VideoTrack::new(),
            audio,
            failure: None,
            raw_bytes: 0,
            scratch: Vec::new(),
            done: false,
            mode,
            pending_video: None,
            video_duration_patches: 0,
            bitstream_format: plan.bitstream_format,
            av1_sequence: plan.av1_sequence,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Number of declared audio tracks, which is the number the artifact reports.
    pub fn audio_track_count(&self) -> usize {
        self.audio.len()
    }

    /// The plan kinds this container declares, in container track order.
    pub fn track_kinds(&self) -> impl Iterator<Item = &TrackKind> {
        self.audio.iter().map(|track| &track.kind)
    }

    pub fn stats(&self) -> MkvAvWriterStats {
        let mut last_end_units = self.video.end_units;
        let mut trimmed_end_units = self.video.end_units;
        let mut audio_packets = 0;
        let mut finished = 0;
        for track in &self.audio {
            audio_packets += track.packets;
            last_end_units = last_end_units.max(track.last_end_units);
            trimmed_end_units = trimmed_end_units.max(track.trimmed_end_units);
            if track.finished {
                finished += 1;
            }
        }
        MkvAvWriterStats {
            video_packets: self.video.packets,
            video_keyframes: self.video.keyframes,
            audio_tracks: self.audio.len() as u32,
            audio_packets,
            audio_finished_tracks: finished,
            raw_payload_bytes: self.raw_bytes,
            first_video_pts_ns: self
                .video
                .first_pts_units
                .map(|units| units * TIMESTAMP_SCALE_NS),
            last_end_ns: last_end_units * TIMESTAMP_SCALE_NS,
            trimmed_end_ns: trimmed_end_units * TIMESTAMP_SCALE_NS,
            retained_bytes_estimate: self.retained_bytes(),
            live_block_durations: self.mode == WriteMode::Live,
            video_duration_patches: self.video_duration_patches,
            video_duration_pending: self.pending_video.is_some(),
            last_checkpoint: self.muxer.as_ref().and_then(Backend::last_checkpoint),
        }
    }

    /// Explicit lifecycle checkpoint, preserving track timestamps, pending live
    /// durations and final audio trim. Normal writes also checkpoint periodically.
    pub fn checkpoint(&mut self) -> Result<DurableCheckpoint, RecorderError> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        let result = self.muxer.as_mut().expect("an active writer").checkpoint();
        match result {
            Ok(point) => Ok(point),
            Err(error) => self.fail(backend_error(
                error,
                "The media checkpoint could not be flushed and synced.",
            )),
        }
    }

    /// True once a packet, the file or the container has failed. A failed writer is
    /// sticky and can never finalize into an artifact.
    pub fn failed(&self) -> bool {
        self.failure.is_some()
    }

    /// Writes one H.264 Annex-B access unit on the single video track.
    ///
    /// In live mode the packet's own duration is only a placeholder until the
    /// next real timestamp arrives: this call first patches the previous video block's
    /// `BlockDuration` with the exact interval to this packet's PTS and then writes
    /// this packet. Only the previous block's offset and PTS are held, never its
    /// payload, and no PTS is ever changed or shifted.
    pub fn write_video(&mut self, packet: EncodedPacket) -> Result<(), RecorderError> {
        if self.mode == WriteMode::Live
            && let Err(error) = self.patch_pending_video_block(packet.pts.0)
        {
            return self.fail(error);
        }
        let pts_ns = packet.pts.0;
        self.write_packet(packet, Slot::Video)?;
        if self.mode == WriteMode::Live
            && let Some(offset) = self
                .muxer
                .as_ref()
                .and_then(Backend::last_block_duration_offset)
        {
            // The block just written keeps a placeholder duration until the next real
            // timestamp. Only its file offset and PTS are remembered.
            self.pending_video = Some(PendingVideoDuration { offset, pts_ns });
        }
        Ok(())
    }

    /// Closes the live video stream: the last video block has no later source
    /// timestamp, so its duration becomes `tail_duration_ns`, explicitly chosen by the
    /// caller from its stop boundary. Finalization never guesses this value. Idempotent, and a
    /// no-op for a completing writer, whose packets already carried their duration.
    pub fn finish_video(&mut self, tail_duration_ns: u64) -> Result<(), RecorderError> {
        if self.mode != WriteMode::Live || self.pending_video.is_none() {
            return Ok(());
        }
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        if self.done {
            return self.fail(request_error(
                "This writer was already finalized.".to_string(),
                RecoveryStatus::Unknown,
            ));
        }
        if tail_duration_ns == 0 {
            return self.fail(request_error(
                "The last video block cannot be closed with a zero duration.".to_string(),
                RecoveryStatus::Unknown,
            ));
        }
        self.patch_pending_video_block_with(tail_duration_ns)
    }

    /// Patches the pending video block with the exact interval to `next_pts_ns`.
    fn patch_pending_video_block(&mut self, next_pts_ns: u64) -> Result<(), RecorderError> {
        let Some(pending) = self.pending_video else {
            return Ok(());
        };
        let Some(interval) = next_pts_ns.checked_sub(pending.pts_ns).filter(|ns| *ns > 0) else {
            return Err(request_error(
                format!(
                    "Video packet PTS {next_pts_ns} ns is not later than the pending video block's {} ns, so the live duration of that block cannot be patched without inventing a timestamp.",
                    pending.pts_ns
                ),
                RecoveryStatus::Unknown,
            ));
        };
        self.patch_pending_video_block_with(interval)
    }

    /// Writes the real duration of the one pending video block and moves that block's
    /// end forward. The block's size, position and timestamp never change.
    fn patch_pending_video_block_with(&mut self, duration_ns: u64) -> Result<(), RecorderError> {
        let Some(pending) = self.pending_video else {
            return Ok(());
        };
        let duration_units = to_units(duration_ns);
        if duration_units == 0 {
            return Err(request_error(
                format!(
                    "A live video duration of {duration_ns} ns is shorter than one {TIMESTAMP_SCALE_NS} ns container unit."
                ),
                RecoveryStatus::Unknown,
            ));
        }
        let pts_units = to_units(pending.pts_ns - self.origin_ns);
        let Some(end_units) = pts_units.checked_add(duration_units) else {
            return Err(request_error(
                "The live video block end exceeds the media timeline.".to_string(),
                RecoveryStatus::Unknown,
            ));
        };
        let result = match self.muxer.as_mut() {
            Some(muxer) => muxer.patch_block_duration(pending.offset, duration_units),
            None => Err(mkwriter::Error::Io(std::io::Error::other(
                "the writer was already finalized",
            ))),
        };
        if let Err(error) = result {
            return Err(backend_error(
                error,
                "The live video block duration could not be patched.",
            ));
        }
        self.muxer
            .as_mut()
            .expect("active writer")
            .clear_block_duration();
        self.pending_video = None;
        // The patched end is this block's real end, so the container's end estimate
        // keeps advancing with the media instead of with a placeholder.
        self.video.end_units = self.video.end_units.max(end_units);
        self.video_duration_patches += 1;
        Ok(())
    }

    /// Writes one Opus packet for the audio track at `track_id`, which is that
    /// track's plan index, exactly as the track's `OpusTrackEncoder` reported it.
    pub fn write_audio(&mut self, packet: EncodedPacket) -> Result<(), RecorderError> {
        let Some(index) = usize::try_from(packet.track_id)
            .ok()
            .filter(|index| *index < self.audio.len())
        else {
            let error = writer_error(
                ErrorCode::InvalidRequest,
                RecoveryStatus::Unknown,
                format!(
                    "Packet track {} is not one of this container's {} audio tracks.",
                    packet.track_id,
                    self.audio.len()
                ),
                AUDIO_ACTION,
            );
            return self.fail(error);
        };
        self.write_packet(packet, Slot::Audio(index))
    }

    /// Closes one audio track with the encoder's own trailing discard padding, in
    /// 48 kHz samples per channel. Patches the last written block in place; the
    /// track then accepts no further packet.
    pub fn finish_audio(
        &mut self,
        track_index: usize,
        discard_padding: u16,
    ) -> Result<(), RecorderError> {
        // All failures preserve the first error, including invalid indices.
        if track_index >= self.audio.len() {
            let error = writer_error(
                ErrorCode::InvalidRequest,
                RecoveryStatus::Unknown,
                format!(
                    "Audio track index {track_index} is not one of this container's {} audio tracks.",
                    self.audio.len()
                ),
                AUDIO_ACTION,
            );
            return self.fail(error);
        }
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        if self.done {
            let error = request_error(
                "This writer was already finalized.".to_string(),
                RecoveryStatus::Unknown,
            );
            return self.fail(error);
        }
        if self.audio[track_index].finished {
            let error = writer_error(
                ErrorCode::InvalidRequest,
                RecoveryStatus::Unknown,
                format!(
                    "Audio track {} was already finished.",
                    self.audio[track_index].name
                ),
                AUDIO_ACTION,
            );
            return self.fail(error);
        }
        if discard_padding > MAX_DISCARD_PADDING_SAMPLES {
            let error = writer_error(
                ErrorCode::InvalidRequest,
                RecoveryStatus::Unknown,
                format!(
                    "A discard padding of {discard_padding} samples exceeds the {MAX_DISCARD_PADDING_SAMPLES} samples one Opus packet can hold."
                ),
                AUDIO_ACTION,
            );
            return self.fail(error);
        }
        // An empty track is not a recording, so it can never be closed as one.
        if self.audio[track_index].accepted == 0 {
            let error = writer_error(
                ErrorCode::InvalidRequest,
                RecoveryStatus::Unknown,
                format!(
                    "Audio track {} received no packet, so it is not a recorded track.",
                    self.audio[track_index].name
                ),
                AUDIO_ACTION,
            );
            return self.fail(error);
        }
        let track = &self.audio[track_index];
        let padding_ns = samples_to_ns(u64::from(discard_padding));
        let delay_ns = samples_to_ns(u64::from(track.pre_skip));
        if track.accepted * 960 <= u64::from(track.pre_skip) + u64::from(discard_padding) {
            return self.fail(request_error(
                "Opus trim leaves no input samples.",
                RecoveryStatus::Unknown,
            ));
        }
        let end_ns = track.last_end_units * TIMESTAMP_SCALE_NS;
        let Some(trimmed_ns) = end_ns.checked_sub(delay_ns + padding_ns) else {
            return self.fail(request_error(
                "Opus trim exceeds the media timeline.",
                RecoveryStatus::Unknown,
            ));
        };
        let offset = track
            .last_padding_offset
            .expect("accepted audio has padding storage");
        if let Err(error) = self
            .muxer
            .as_mut()
            .expect("active writer")
            .patch_padding(offset, padding_ns as i64)
        {
            return self.fail(backend_error(
                error,
                "The final audio padding could not be patched.",
            ));
        }
        let track = &mut self.audio[track_index];
        track.discard_padding_samples = discard_padding;
        track.discard_padding_units = to_units(padding_ns);
        track.trimmed_end_units = to_units(trimmed_ns);
        track.finished = true;
        Ok(())
    }

    fn metadata_bytes(&self) -> u64 {
        self.av1_sequence
            .as_ref()
            .map_or(0, |bytes| bytes.capacity() as u64)
            + (self.audio.capacity() * std::mem::size_of::<AudioTrack>()) as u64
            + self
                .audio
                .iter()
                .map(|track| {
                    track.name.capacity() as u64
                        + match &track.kind {
                            TrackKind::MainMix => 0,
                            TrackKind::Clean(id) => id.0.capacity() as u64,
                        }
                })
                .sum::<u64>()
    }

    fn retained_bytes(&self) -> u64 {
        self.muxer.as_ref().map_or(0, Backend::retained_bytes)
            + self.scratch.capacity() as u64
            + self.metadata_bytes()
            + FINALIZE_SCRATCH_BYTES
    }

    fn fail<T>(&mut self, error: RecorderError) -> Result<T, RecorderError> {
        Err(self.failure.get_or_insert(error).clone())
    }

    /// Decides the frame one accepted packet becomes, and updates the track's own
    /// time and payload counters. This is the only place that writes a packet.
    fn write_frame(
        &mut self,
        slot: Slot,
        prepared: PreparedFrame,
        data: &[u8],
    ) -> Result<(), RecorderError> {
        let pts_units = prepared.timestamp_ns / TIMESTAMP_SCALE_NS;
        let live_video = self.mode == WriteMode::Live && matches!(slot, Slot::Video);
        let frame = Frame {
            track: prepared.track,
            timestamp_ns: prepared.timestamp_ns,
            duration_ns: Some(prepared.duration_ns),
            data,
            keyframe: prepared.keyframe,
            discard_padding_ns: prepared.discard_padding_ns,
            ..Frame::default()
        };
        let result = match self.muxer.as_mut() {
            Some(muxer) => muxer.write_frame(frame, live_video),
            None => Err(mkwriter::Error::Io(std::io::Error::other(
                "the writer was already finalized",
            ))),
        };
        if let Err(error) = result {
            return self.fail(backend_error(
                error,
                "The container rejected a packet that had passed validation.",
            ));
        }
        let end_units = prepared.end_units;
        match slot {
            Slot::Video => {
                self.video.packets += 1;
                self.video.last_pts_units = Some(pts_units);
                self.video.first_pts_units.get_or_insert(pts_units);
                if !live_video {
                    self.video.end_units = self.video.end_units.max(end_units);
                }
                self.video.longest_packet_bytes =
                    self.video.longest_packet_bytes.max(data.len() as u64);
                if prepared.keyframe {
                    self.video.keyframes += 1;
                }
            }
            Slot::Audio(index) => {
                let track = &mut self.audio[index];
                track.packets += 1;
                track.accepted += 1;
                track.last_padding_offset =
                    self.muxer.as_ref().and_then(Backend::last_padding_offset);
                track.last_pts_units = Some(pts_units);
                track.last_end_units = end_units;
                track.trimmed_end_units = end_units;
                track.longest_packet_bytes = track.longest_packet_bytes.max(data.len() as u64);
            }
        }
        // The pre-write projection bounds the backend's growth, so this second check
        // only fires when the allocator grew a buffer past that estimate.
        if self.retained_bytes() > MAX_RETAINED_BYTES {
            let error = backend_limit_error(format!(
                "The container writer now retains about {} bytes, above the {MAX_RETAINED_BYTES} byte budget.",
                self.retained_bytes()
            ));
            return self.fail(error);
        }
        Ok(())
    }

    fn write_packet(&mut self, packet: EncodedPacket, slot: Slot) -> Result<(), RecorderError> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        if self.done {
            let error = request_error(
                "This writer was already finalized.".to_string(),
                RecoveryStatus::Unknown,
            );
            return self.fail(error);
        }
        if let Slot::Audio(index) = slot
            && self.audio[index].finished
        {
            let error = writer_error(
                ErrorCode::InvalidRequest,
                RecoveryStatus::Unknown,
                format!(
                    "Audio track {} is finished and rejects further packets.",
                    self.audio[index].name
                ),
                AUDIO_ACTION,
            );
            return self.fail(error);
        }
        // The first packet of every declared track is in hand before the first byte of a
        // live container is written, so no track is missing from its opening clusters. That
        // invariant belongs to the live owner (`AvRecording`), which holds every packet
        // until the last announced track delivered its first one; this writer states the
        // requirement rather than attempting a gate it could not honour without dropping
        // the packet in front of it.
        //
        // A rejected packet is a sticky failure: the writer never continues past one.
        let prepared = match self.check_packet(&packet, slot) {
            Ok(prepared) => prepared,
            Err(error) => return self.fail(error),
        };

        // Charge the whole retained footprint before allocating or mutating anything:
        // the incoming allocation, this packet's converted block, the bounded cue index
        // and the fixed header/scratch buffers together.
        if matches!(slot, Slot::Video)
            && let Some(sequence) = self.av1_sequence.as_deref()
        {
            match crate::av1::validate_packet(&packet.data, Some(sequence)) {
                Ok(independent) if independent == packet.keyframe => {}
                result => {
                    return self.fail(request_error(
                        format!("AV1 packet/independent keyframe validation failed: {result:?}"),
                        RecoveryStatus::Recoverable,
                    ));
                }
            }
        }
        let converted = match slot {
            Slot::Video => match video_access_unit_size(self.bitstream_format, &packet.data) {
                Ok(size) => size,
                Err(defect) => {
                    let error = request_error(
                        format!("The packet is not a valid Annex-B access unit: {defect}"),
                        RecoveryStatus::Unknown,
                    );
                    return self.fail(error);
                }
            },
            Slot::Audio(_) => packet.data.len(),
        };
        let scratch = match slot {
            Slot::Video => self.scratch.capacity().max(converted),
            Slot::Audio(_) => self.scratch.capacity(),
        };
        let projected = packet.data.capacity() as u64
            + scratch as u64
            + self.metadata_bytes()
            + self
                .muxer
                .as_ref()
                .map_or(0, |backend| backend.projected_bytes(converted + 16))
            + FINALIZE_SCRATCH_BYTES;
        if projected > MAX_RETAINED_BYTES {
            return self.fail(backend_limit_error(
                "Packet would exceed the writer memory budget.",
            ));
        }

        match slot {
            Slot::Video => {
                // No video packet is ever held: an access unit needs no trailing trim.
                let mut scratch = std::mem::take(&mut self.scratch);
                if let Err(defect) =
                    video_access_unit(self.bitstream_format, &packet.data, &mut scratch)
                {
                    self.scratch = scratch;
                    let error = request_error(
                        format!("Invalid Annex-B: {defect}"),
                        RecoveryStatus::Unknown,
                    );
                    return self.fail(error);
                }
                if self.bitstream_format == BitstreamFormat::HevcAnnexB {
                    match crate::hevc::validate_recovered_access_unit(&scratch) {
                        Ok(independent) if independent == packet.keyframe => {}
                        result => {
                            self.scratch = scratch;
                            return self.fail(request_error(
                                format!("HEVC packet/keyframe validation failed: {result:?}"),
                                RecoveryStatus::Recoverable,
                            ));
                        }
                    }
                }
                let result = self.write_frame(slot, prepared, &scratch);
                self.scratch = scratch;
                match result {
                    Ok(()) => {}
                    Err(error) => return self.fail(error),
                }
            }
            Slot::Audio(_) => self.write_frame(slot, prepared, &packet.data)?,
        }
        if let Slot::Video = slot {
            self.video.packet_track_id.get_or_insert(packet.track_id);
        }
        self.raw_bytes += packet.data.len() as u64;
        if self.retained_bytes() > MAX_RETAINED_BYTES {
            let error = backend_limit_error(format!(
                "The container writer now retains about {} bytes, above the {MAX_RETAINED_BYTES} byte budget.",
                self.retained_bytes()
            ));
            return self.fail(error);
        }
        Ok(())
    }

    /// Validates one packet without changing any state, so a rejected packet leaves
    /// the writer exactly as it was apart from the sticky failure.
    fn check_packet(
        &self,
        packet: &EncodedPacket,
        slot: Slot,
    ) -> Result<PreparedFrame, RecorderError> {
        let name = match slot {
            Slot::Video => "video",
            Slot::Audio(index) => self.audio[index].name.as_str(),
        };
        let Some(offset) = packet.pts.0.checked_sub(self.origin_ns) else {
            let error = request_error(
                format!(
                    "Packet PTS {} ns is earlier than the shared session origin {} ns.",
                    packet.pts.0, self.origin_ns
                ),
                RecoveryStatus::Unknown,
            );
            return Err(error);
        };
        if packet.data.is_empty() {
            let error = request_error(
                format!("An empty {name} packet cannot be written."),
                RecoveryStatus::Unknown,
            );
            return Err(error);
        }
        if packet.data.len() > MAX_PACKET_BYTES {
            let error = request_error(
                format!(
                    "A packet of {} bytes exceeds the {MAX_PACKET_BYTES} byte limit.",
                    packet.data.len()
                ),
                RecoveryStatus::Unknown,
            );
            return Err(error);
        }
        if packet.dts != packet.pts {
            let error = request_error(
                format!(
                    "Packet DTS {} ns differs from PTS {} ns; this writer stores DTS equal to PTS and does not reorder packets.",
                    packet.dts.0, packet.pts.0
                ),
                RecoveryStatus::Unknown,
            );
            return Err(error);
        }
        let pts_units = to_units(offset);
        let Some(container_pts_ns) = pts_units.checked_mul(TIMESTAMP_SCALE_NS) else {
            let error = request_error(
                format!("Packet PTS {offset} ns is beyond the container's representable range."),
                RecoveryStatus::Unknown,
            );
            return Err(error);
        };
        // Each track stays in decode/presentation order. Another track may arrive
        // later with an earlier PTS; the backend encodes that block's actual time
        // relative to its cluster, rolling the cluster when i16 cannot hold it.
        // Neither packet data nor timestamps are retained or shifted here.
        let previous_on_track = match slot {
            Slot::Video => self.video.last_pts_units,
            Slot::Audio(index) => self.audio[index].last_pts_units,
        };
        if let Some(previous) = previous_on_track
            && pts_units <= previous
        {
            let error = request_error(
                format!(
                    "Packet PTS {offset} ns maps to container unit {pts_units}, which is not later than container unit {previous} already written on the same track; a container with {TIMESTAMP_SCALE_NS} ns units cannot hold two packets this close together."
                ),
                RecoveryStatus::Unknown,
            );
            return Err(error);
        }
        if packet.duration_ns == 0 && !(self.mode == WriteMode::Live && matches!(slot, Slot::Video))
        {
            // A live video packet's own duration is only a placeholder: its real
            // duration is the next packet's real timestamp, so a zero here invents
            // nothing. Every other packet must describe its own duration.
            let error = request_error(
                format!("A {name} packet with a zero duration cannot describe a packet."),
                RecoveryStatus::Unknown,
            );
            return Err(error);
        }
        let duration_units = to_units(packet.duration_ns);
        if duration_units == 0 {
            let error = request_error(
                format!(
                    "A duration of {} ns is shorter than one {TIMESTAMP_SCALE_NS} ns container unit.",
                    packet.duration_ns
                ),
                RecoveryStatus::Unknown,
            );
            return Err(error);
        }
        let Some(container_duration_ns) = duration_units.checked_mul(TIMESTAMP_SCALE_NS) else {
            let error = request_error(
                format!(
                    "A duration of {} ns is beyond the container's representable range.",
                    packet.duration_ns
                ),
                RecoveryStatus::Unknown,
            );
            return Err(error);
        };
        if pts_units
            .checked_add(duration_units)
            .filter(|end| end.checked_mul(TIMESTAMP_SCALE_NS).is_some())
            .is_none()
        {
            let error = request_error(
                "Packet end exceeds the media timeline.".to_string(),
                RecoveryStatus::Unknown,
            );
            return Err(error);
        }
        let (track, keyframe) = match slot {
            Slot::Video => {
                if self
                    .video
                    .packet_track_id
                    .is_some_and(|id| id != packet.track_id)
                {
                    return Err(request_error(
                        "Video packet changed track identity.",
                        RecoveryStatus::Unknown,
                    ));
                }
                if self.video.packets == 0 && !packet.keyframe {
                    let error = request_error(
                        "The first video packet is not a keyframe, so the file would start without a random access point."
                            .to_string(),
                        RecoveryStatus::Unknown,
                    );
                    return Err(error);
                }
                if packet.keyframe && self.video.keyframes + 1 > MAX_KEYFRAMES {
                    let error = backend_limit_error(format!(
                        "Refusing keyframe {} because this writer indexes at most {MAX_KEYFRAMES} of them.",
                        self.video.keyframes + 1
                    ));
                    return Err(error);
                }
                (VIDEO_TRACK_NUMBER, packet.keyframe)
            }
            Slot::Audio(_) => {
                if packet.duration_ns != OPUS_PACKET_NS {
                    let error = writer_error(
                        ErrorCode::InvalidRequest,
                        RecoveryStatus::Unknown,
                        format!(
                            "An Opus packet of {} ns is not the {OPUS_PACKET_NS} ns frame this writer declares.",
                            packet.duration_ns
                        ),
                        AUDIO_ACTION,
                    );
                    return Err(error);
                }
                // An Opus packet is a random access point; the container adds the
                // codec's own pre-roll for seeking.
                (self.audio_index_number(slot), true)
            }
        };
        Ok(PreparedFrame {
            track,
            timestamp_ns: container_pts_ns,
            duration_ns: container_duration_ns,
            end_units: pts_units + duration_units,
            keyframe,
            discard_padding_ns: None,
        })
    }

    fn audio_index_number(&self, slot: Slot) -> u64 {
        match slot {
            Slot::Video => VIDEO_TRACK_NUMBER,
            Slot::Audio(index) => self.audio[index].number,
        }
    }

    /// Finishes every stream. Success only after all declared audio tracks ended, the
    /// container was patched, flushed, synced and closed.
    pub fn finalize(mut self) -> Result<RecordingArtifact, RecorderError> {
        if let Some(error) = self.failure.take() {
            // The container is already incomplete. Dropping it without finalizing
            // keeps the partial file for diagnosis and never reports an artifact.
            self.muxer = None;
            return Err(error);
        }
        if self.video.packets == 0 {
            let error = request_error(
                format!(
                    "{} holds no video packet, so it is not a recording.",
                    self.path.display()
                ),
                RecoveryStatus::Unknown,
            );
            return self.fail(error);
        }
        for index in 0..self.audio.len() {
            let track = &self.audio[index];
            if track.accepted == 0 {
                let error = writer_error(
                    ErrorCode::InvalidRequest,
                    RecoveryStatus::Unknown,
                    format!(
                        "Audio track {} received no packet, so the recording is incomplete.",
                        track.name
                    ),
                    AUDIO_ACTION,
                );
                return self.fail(error);
            }
            if !track.finished {
                let error = writer_error(
                    ErrorCode::InvalidRequest,
                    RecoveryStatus::Unknown,
                    format!(
                        "Audio track {} was never finished, so its trim and real end are unknown.",
                        track.name
                    ),
                    AUDIO_ACTION,
                );
                return self.fail(error);
            }
        }
        if self.pending_video.is_some() {
            return self.fail(request_error(
                "The live video stream must be closed at the Stop boundary before finalization."
                    .to_string(),
                RecoveryStatus::Unknown,
            ));
        }
        let Some(muxer) = self.muxer.take() else {
            return Err(request_error(
                "This writer was already finalized.".to_string(),
                RecoveryStatus::Unknown,
            ));
        };
        self.done = true;
        let adapter = match muxer.finalize(
            &[VIDEO_TRACK_NUMBER],
            Some(self.stats().trimmed_end_ns / TIMESTAMP_SCALE_NS),
        ) {
            Ok(adapter) => adapter,
            Err(error) => {
                return Err(backend_error(
                    error,
                    "The container could not be finalized.",
                ));
            }
        };
        let SinkAdapter(target) = adapter;
        target.finish().map_err(|error| {
            storage_error(
                &error,
                "The finalized container could not be flushed and synced to disk.",
            )
        })?;

        let stats = self.stats();
        Ok(RecordingArtifact {
            path: self.path.to_string_lossy().into_owned(),
            duration_ms: stats.trimmed_end_ns as f64 / 1_000_000.0,
            audio_tracks: self.audio.len() as u32,
        })
    }
}

/// Validates the declared audio tracks before any file is created.
fn prepare_audio_tracks(tracks: &[AvAudioTrack]) -> Result<Vec<PlannedAudioTrack>, RecorderError> {
    if tracks.len() > MAX_AUDIO_TRACKS {
        return Err(writer_error(
            ErrorCode::InvalidRequest,
            RecoveryStatus::NoRecording,
            format!(
                "{} audio tracks were declared, more than the {MAX_AUDIO_TRACKS} this writer accepts.",
                tracks.len()
            ),
            PLAN_ACTION,
        ));
    }
    let header_bytes = tracks.len().checked_mul(256).ok_or_else(|| {
        request_error("Audio header size overflows.", RecoveryStatus::NoRecording)
    })?;
    if header_bytes > 4 * 1024 * 1024 {
        return Err(request_error(
            "Audio header exceeds the recovery Tracks memory budget.",
            RecoveryStatus::NoRecording,
        ));
    }
    let mut planned = Vec::with_capacity(tracks.len());
    let mut seen_main = false;
    let mut clean_count = 0;
    for (index, track) in tracks.iter().enumerate() {
        let expected_name = match &track.kind {
            TrackKind::MainMix => {
                if index != 0 {
                    return Err(writer_error(
                        ErrorCode::InvalidRequest,
                        RecoveryStatus::NoRecording,
                        "The main mix is the first audio track of the plan; this plan puts it later."
                            .to_string(),
                        PLAN_ACTION,
                    ));
                }
                if seen_main {
                    return Err(writer_error(
                        ErrorCode::InvalidRequest,
                        RecoveryStatus::NoRecording,
                        "The audio plan describes more than one main mix.".to_string(),
                        PLAN_ACTION,
                    ));
                }
                seen_main = true;
                "main".to_string()
            }
            TrackKind::Clean(id) => {
                if id.0.is_empty() {
                    return Err(writer_error(
                        ErrorCode::InvalidRequest,
                        RecoveryStatus::NoRecording,
                        "A clean audio track has no source identifier.".to_string(),
                        PLAN_ACTION,
                    ));
                }
                clean_count += 1;
                format!("clean-{clean_count}")
            }
        };
        planned.push(PlannedAudioTrack {
            channels: track.channels,
            number: audio_track_number(index)?,
            name: expected_name,
            kind: track.kind.clone(),
            pre_skip: track.pre_skip,
        });
    }
    Ok(planned)
}

/// Container track numbers follow the plan order: audio index `i` is `i + 2`.
fn audio_track_number(index: usize) -> Result<u64, RecorderError> {
    let offset = u64::try_from(index).map_err(|_| {
        writer_error(
            ErrorCode::InvalidRequest,
            RecoveryStatus::NoRecording,
            "The audio track index does not fit the container track number.".to_string(),
            PLAN_ACTION,
        )
    })?;
    let Some(number) = VIDEO_TRACK_NUMBER
        .checked_add(1)
        .and_then(|first| first.checked_add(offset))
    else {
        return Err(writer_error(
            ErrorCode::InvalidRequest,
            RecoveryStatus::NoRecording,
            "The audio track number overflows the container's track range.".to_string(),
            PLAN_ACTION,
        ));
    };
    if number > VIDEO_TRACK_NUMBER + MAX_AUDIO_TRACKS as u64 {
        return Err(writer_error(
            ErrorCode::InvalidRequest,
            RecoveryStatus::NoRecording,
            format!(
                "Audio track {index} has no container track number inside the {MAX_AUDIO_TRACKS} track audio range."
            ),
            PLAN_ACTION,
        ));
    }
    Ok(number)
}

/// Stable per-track identifier: output must not change between runs.
pub(super) fn track_uid(number: u64) -> u64 {
    0x4652_414D_4500_0000 + number
}

/// The video track keeps the accepted `0x4652_414D_4500_0001` identifier.
fn video_track_uid() -> u64 {
    track_uid(VIDEO_TRACK_NUMBER)
}

/// The 19-byte `OpusHead` for this track's real pre-skip, version 1, stereo, with no
/// channel mapping table. The pre-skip is stored once, here and in `CodecDelay`.
#[cfg(test)]
fn opus_head(pre_skip: u16) -> Vec<u8> {
    opus_head_channels(pre_skip, OPUS_CHANNELS)
}

fn opus_head_channels(pre_skip: u16, channels: u16) -> Vec<u8> {
    let mut head = Vec::with_capacity(OPUS_HEAD_BYTES);
    head.extend_from_slice(b"OpusHead");
    head.push(1);
    head.push(channels as u8);
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&(OPUS_SAMPLE_RATE as u32).to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes());
    head.push(0);
    debug_assert_eq!(head.len(), OPUS_HEAD_BYTES);
    head
}

/// Exact sample count to nanoseconds at the fixed 48 kHz rate. Every value this
/// writer stores is floored to the nearest nanosecond (less than one ns error).
fn samples_to_ns(samples: u64) -> u64 {
    samples.saturating_mul(1_000_000_000) / OPUS_SAMPLE_RATE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mkv::test_support::FailingTarget;
    use frame_contracts::AudioSourceId;
    use frame_encode::BitstreamFormat;
    use std::num::NonZeroU32;

    const SPS: [u8; 8] = [0x67, 0x64, 0x00, 0x3C, 0xAC, 0x2B, 0x40, 0x14];
    const PPS: [u8; 4] = [0x68, 0xEE, 0x3C, 0xB0];
    const PRE_SKIP: u16 = 312;
    const FRAME_NS: u64 = OPUS_PACKET_NS;

    fn stream_info() -> EncodedStreamInfo {
        let mut codec_config = Vec::new();
        codec_config.extend_from_slice(&[0, 0, 0, 1]);
        codec_config.extend_from_slice(&SPS);
        codec_config.extend_from_slice(&[0, 0, 0, 1]);
        codec_config.extend_from_slice(&PPS);
        EncodedStreamInfo {
            capability: frame_encode::EncoderCapability {
                id: "test".into(),
                codec: "h264".into(),
                hardware: false,
            },
            width: NonZeroU32::new(64).expect("non-zero"),
            height: NonZeroU32::new(48).expect("non-zero"),
            nominal_rate_num: NonZeroU32::new(30).expect("non-zero"),
            nominal_rate_den: NonZeroU32::new(1).expect("non-zero"),
            bitstream_format: BitstreamFormat::H264AnnexB,
            colour: None,
            codec_config,
            allows_reordering: false,
        }
    }

    fn video_packet(pts_ns: u64, keyframe: bool) -> EncodedPacket {
        EncodedPacket {
            track_id: 0,
            pts: MediaTimestamp(pts_ns),
            dts: MediaTimestamp(pts_ns),
            duration_ns: 20_000_000,
            keyframe,
            data: if keyframe {
                vec![0, 0, 0, 1, 0x65, 0x11, 0x22, 0x33]
            } else {
                vec![0, 0, 0, 1, 0x41, 0x44, 0x55]
            },
        }
    }

    fn audio_packet(track_id: u32, pts_ns: u64) -> EncodedPacket {
        EncodedPacket {
            track_id,
            pts: MediaTimestamp(pts_ns),
            dts: MediaTimestamp(pts_ns),
            duration_ns: FRAME_NS,
            keyframe: true,
            data: vec![0xFC, 0x12, 0x34],
        }
    }

    fn plans() -> Vec<TrackPlan> {
        vec![
            TrackPlan {
                kind: TrackKind::MainMix,
                sources: vec![AudioSourceId("a".into())],
            },
            TrackPlan {
                kind: TrackKind::Clean(AudioSourceId("a".into())),
                sources: vec![AudioSourceId("a".into())],
            },
            TrackPlan {
                kind: TrackKind::Clean(AudioSourceId("b".into())),
                sources: vec![AudioSourceId("b".into())],
            },
        ]
    }

    fn av_tracks(plans: &[TrackPlan]) -> Vec<AvAudioTrack> {
        plans
            .iter()
            .map(|plan| AvAudioTrack::from_plan(plan, PRE_SKIP).expect("named track"))
            .collect()
    }

    fn writer_with_target(plans: &[TrackPlan], limit: usize) -> MkvAvWriter {
        writer_and_target(plans, limit).0
    }

    #[test]
    fn a_checkpoint_failure_is_sticky_and_cannot_finalize() {
        let (mut writer, target) = writer_and_target(&[], usize::MAX);
        writer.write_video(video_packet(0, true)).unwrap();
        let before = writer.stats().last_checkpoint.unwrap();
        writer.write_video(video_packet(FRAME_NS, false)).unwrap();
        target.fail_checkpoint();
        let error = writer.checkpoint().unwrap_err();
        assert!(writer.failed());
        assert_eq!(writer.stats().last_checkpoint, Some(before));
        assert_eq!(
            writer
                .write_video(video_packet(2 * FRAME_NS, false))
                .unwrap_err()
                .message,
            error.message
        );
        assert_eq!(writer.finalize().unwrap_err().message, error.message);
    }

    /// The same writer plus its injected target, so a test can raise the limit after
    /// the header and first packets have legitimately filled part of it.
    fn writer_and_target(plans: &[TrackPlan], limit: usize) -> (MkvAvWriter, FailingTarget) {
        writer_and_target_in(plans, limit, WriteMode::Complete)
    }

    /// The same writer in either mode. A live writer never writes before every declared
    /// audio track delivered its first packet, so the live tests use one track.
    fn writer_and_target_in(
        plans: &[TrackPlan],
        limit: usize,
        mode: WriteMode,
    ) -> (MkvAvWriter, FailingTarget) {
        let plan = plan_stream(&stream_info()).expect("valid stream");
        let tracks = prepare_audio_tracks(&av_tracks(plans)).expect("valid tracks");
        let target = FailingTarget::new(limit);
        let writer = MkvAvWriter::with_target(
            PathBuf::from("injected.mkv"),
            Box::new(target.clone()),
            plan,
            tracks,
            MediaTimestamp(0),
            mode,
        )
        .expect("the header fits the injected target");
        (writer, target)
    }

    /// A three-track container with one packet on every track at the same timestamp,
    /// which is legal across tracks.
    fn three_track_writer(limit: usize) -> MkvAvWriter {
        let mut writer = writer_with_target(&plans(), limit);
        assert_eq!(writer.audio_track_count(), 3);
        assert_eq!(
            writer.track_kinds().cloned().collect::<Vec<_>>(),
            av_tracks(&plans())
                .iter()
                .map(|track| track.kind().clone())
                .collect::<Vec<_>>()
        );
        writer.write_video(video_packet(0, true)).expect("video");
        for index in 0..3u32 {
            writer
                .write_audio(audio_packet(index, 0))
                .expect("audio at the video's timestamp");
        }
        writer
    }

    /// The pre-skip the declared Opus track actually carries into the container.
    fn upheld_pre_skip() -> u16 {
        let writer = writer_with_target(&plans(), usize::MAX);
        writer.audio[0].pre_skip
    }

    #[test]
    fn plan_names_numbers_and_pre_skip_follow_the_audio_plan() {
        let tracks = av_tracks(&plans());
        let planned = prepare_audio_tracks(&tracks).unwrap();
        assert_eq!(
            planned.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["main", "clean-1", "clean-2"]
        );
        assert_eq!(tracks[0].pre_skip(), PRE_SKIP);
        assert!(matches!(tracks[0].kind(), TrackKind::MainMix));
        // The declared pre-skip also reaches the container header, unchanged.
        assert_eq!(upheld_pre_skip(), PRE_SKIP);
        assert_eq!(audio_track_number(0).unwrap(), 2);
        assert_eq!(audio_track_number(16).unwrap(), 18);
        assert!(audio_track_number(MAX_AUDIO_TRACKS).is_err());
        // Only the main mix is the plan's first track, so only it is the default.
        let head = opus_head(PRE_SKIP);
        assert_eq!(&head[..8], b"OpusHead");
        assert_eq!(head[8], 1, "OpusHead version");
        assert_eq!(head[9], 2, "stereo");
        assert_eq!(u16::from_le_bytes([head[10], head[11]]), PRE_SKIP);
        assert_eq!(
            u32::from_le_bytes([head[12], head[13], head[14], head[15]]),
            OPUS_SAMPLE_RATE as u32
        );
        assert_eq!(samples_to_ns(u64::from(PRE_SKIP)), 6_500_000);
        assert_eq!(OPUS_SEEK_PRE_ROLL_NS, 80_000_000);
    }

    #[test]
    fn more_than_seventeen_audio_tracks_are_supported() {
        let plans: Vec<_> = (0..40)
            .map(|i| TrackPlan {
                kind: TrackKind::Clean(AudioSourceId(format!("s{i:03}"))),
                sources: vec![AudioSourceId(format!("s{i:03}"))],
            })
            .collect();
        assert_eq!(prepare_audio_tracks(&av_tracks(&plans)).unwrap().len(), 40);
        assert_ne!(track_uid(2), track_uid(65538));
    }

    #[test]
    fn a_main_mix_after_a_clean_stem_is_refused() {
        let mut plans = plans();
        plans.swap(0, 1);
        let error =
            prepare_audio_tracks(&av_tracks(&plans)).expect_err("the main mix must come first");
        assert!(error.message.contains("main mix is the first"));
    }

    #[test]
    fn one_timestamp_is_legal_across_tracks_and_never_twice_on_one_track() {
        let mut writer = writer_with_target(&plans(), usize::MAX);
        writer.write_video(video_packet(0, true)).expect("first");
        // Three tracks, one timestamp: audio index 1 and 2 are different tracks.
        writer
            .write_audio(audio_packet(0, 0))
            .expect("the same timestamp on another track");
        writer
            .write_audio(audio_packet(1, 0))
            .expect("a third track at the same timestamp");
        // The same track cannot take a second packet inside one container unit.
        let error = writer
            .write_audio(audio_packet(0, 0))
            .expect_err("a repeated timestamp on one track is refused");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(error.message.contains("same track"), "{}", error.message);
        assert!(writer.failed(), "a rejected packet is a sticky failure");
        assert!(writer.finalize().is_err());

        // A nearby but distinct timestamp on the same track is fine.
        let mut writer = writer_with_target(&plans(), usize::MAX);
        writer.write_video(video_packet(0, true)).expect("video");
        writer.write_audio(audio_packet(0, 0)).expect("main at 0");
        writer
            .write_audio(audio_packet(0, FRAME_NS))
            .expect("main at 20 ms");
        let error = writer
            .write_audio(audio_packet(0, FRAME_NS + 400))
            .expect_err("400 ns rounds onto the previous packet");
        assert!(
            error.message.contains("cannot hold two packets"),
            "{}",
            error.message
        );
    }

    #[test]
    fn an_earlier_timestamp_on_another_track_is_written_without_reordering() {
        let mut writer = writer_with_target(&plans(), usize::MAX);
        writer
            .write_video(video_packet(0, true))
            .expect("video at 0");
        writer
            .write_audio(audio_packet(0, FRAME_NS))
            .expect("audio at 20 ms");
        writer
            .write_audio(audio_packet(1, 0))
            .expect("a different track may arrive after a later PTS");
        assert!(!writer.failed());
        assert_eq!(writer.stats().audio_packets, 2);
    }

    #[test]
    fn a_container_rejects_audio_for_an_undeclared_track() {
        let mut writer = writer_with_target(&plans(), usize::MAX);
        let error = writer
            .write_audio(audio_packet(3, 0))
            .expect_err("track index 3 is not declared");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(error.message.contains("3 audio tracks"));
    }

    #[test]
    fn an_unfinished_or_empty_audio_track_never_produces_an_artifact() {
        let mut writer = three_track_writer(usize::MAX);
        writer.finish_audio(0, 0).expect("main finishes");
        writer.finish_audio(1, 0).expect("clean 1 finishes");
        let error = writer.finalize().expect_err("clean 2 was never finished");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(error.message.contains("never finished"));

        let mut writer = writer_with_target(&plans(), usize::MAX);
        writer.write_video(video_packet(0, true)).expect("video");
        writer.write_audio(audio_packet(0, 0)).expect("main");
        writer.finish_audio(0, 0).expect("main finishes");
        let error = writer
            .finalize()
            .expect_err("an empty declared track is not a recording");
        assert!(error.message.contains("received no packet"));

        let mut writer = writer_with_target(&plans(), usize::MAX);
        writer.write_video(video_packet(0, true)).expect("video");
        let error = writer
            .finish_audio(0, 0)
            .expect_err("an empty track cannot be finished");
        assert!(
            error.message.contains("received no packet") || error.message.contains("does not fit")
        );
        let error = writer
            .finalize()
            .expect_err("the declared tracks still hold no packets");
        assert!(error.message.contains("received no packet"));
    }

    #[test]
    fn the_trimmed_end_uses_the_last_packet_and_its_discard_padding() {
        let mut writer = writer_with_target(&plans(), usize::MAX);
        writer.write_video(video_packet(0, true)).expect("video");
        for index in 0..3u64 {
            writer
                .write_audio(audio_packet(0, index * FRAME_NS))
                .expect("main");
        }
        // Every accepted packet is already written.
        assert_eq!(writer.stats().audio_packets, 3);
        writer
            .finish_audio(0, 480)
            .expect("main finishes with trim");
        let stats = writer.stats();
        assert_eq!(stats.audio_packets, 3);
        assert_eq!(stats.audio_finished_tracks, 1);
        // 60 ms minus 6.5 ms codec delay and 10 ms trailing padding.
        assert_eq!(stats.last_end_ns, 60_000_000);
        assert_eq!(stats.trimmed_end_ns, 43_500_000);
        assert_eq!(writer.audio[0].discard_padding_samples, 480);
    }

    #[test]
    fn a_discard_padding_larger_than_one_packet_is_refused() {
        let mut writer = writer_with_target(&plans(), usize::MAX);
        writer.write_video(video_packet(0, true)).expect("video");
        writer.write_audio(audio_packet(0, 0)).expect("main");
        let error = writer
            .finish_audio(0, u16::MAX)
            .expect_err("padding cannot exceed one packet");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(error.message.contains("exceeds"));
    }

    #[test]
    fn finishing_twice_and_an_unknown_index_are_refused() {
        let mut writer = three_track_writer(usize::MAX);
        writer.finish_audio(0, 0).expect("main finishes");
        assert!(
            writer
                .finish_audio(0, 0)
                .expect_err("a finished track stays finished")
                .message
                .contains("already finished")
        );
        assert!(
            writer
                .finish_audio(9, 0)
                .expect_err("sticky failure precedes a later bad index")
                .message
                .contains("already finished"),
            "the first failure is preserved"
        );
        let error = writer
            .write_audio(audio_packet(0, FRAME_NS))
            .expect_err("a finished track rejects packets");
        assert!(error.message.contains("finished"), "{}", error.message);
    }

    #[test]
    fn an_audio_packet_of_the_wrong_length_is_refused() {
        let mut writer = writer_with_target(&plans(), usize::MAX);
        writer.write_video(video_packet(0, true)).expect("video");
        let mut packet = audio_packet(0, 0);
        packet.duration_ns = 10_000_000;
        let error = writer.write_audio(packet).expect_err("not a 20 ms frame");
        assert!(error.message.contains("20000000"), "{}", error.message);
    }

    #[test]
    fn a_container_without_video_never_finalizes() {
        let writer = writer_with_target(&[], usize::MAX);
        let error = writer
            .finalize()
            .expect_err("a container without video is not a recording");
        assert!(error.message.contains("no video packet"));
    }

    #[test]
    fn audio_packets_do_not_grow_the_video_cue_index() {
        let mut writer = writer_with_target(&plans()[..1], usize::MAX);
        writer.write_video(video_packet(0, true)).unwrap();
        writer.write_audio(audio_packet(0, 0)).unwrap();
        let retained = writer.retained_bytes();
        for i in 1..64 {
            writer.write_audio(audio_packet(0, i * FRAME_NS)).unwrap();
            assert_eq!(writer.retained_bytes(), retained);
        }
        writer.finish_audio(0, 480).unwrap();
        assert_eq!(writer.finalize().unwrap().duration_ms, 1263.5);
    }

    #[test]
    fn the_injected_target_failure_is_sticky() {
        // The header and the first access unit fit; the limit is then lowered so that
        // the first audio block the writer really flushes cannot fit.
        let (mut writer, target) = writer_and_target(&plans(), usize::MAX);
        writer
            .write_video(video_packet(0, true))
            .expect("the header and first packet fit");
        target.set_limit(target.limit() + 10);
        let error = writer
            .write_audio(audio_packet(0, 0))
            .expect_err("the injected target refuses this write");
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(writer.failed(), "the injected target failure is sticky");
        let repeated = writer
            .write_video(video_packet(FRAME_NS, false))
            .expect_err("the writer stays failed");
        assert_eq!(repeated.message, error.message);
        assert_eq!(writer.finalize().unwrap_err().message, error.message);
    }

    /// Every `(TrackNumber, BlockDuration)` pair in file order, read with the library's
    /// own element reader so a matching byte pair inside a block cannot count.
    fn block_durations(bytes: &[u8]) -> Vec<(u64, u64)> {
        fn block_track(body: &[u8]) -> u64 {
            let first = body[0];
            let len = first.leading_zeros() as usize + 1;
            let mask: u8 = if len >= 8 { 0 } else { 0xFF >> len };
            let mut track = u64::from(first & mask);
            for byte in &body[1..len] {
                track = (track << 8) | u64::from(*byte);
            }
            track
        }
        let mut out = Vec::new();
        let segment = mkwriter::reader::children(bytes)
            .into_iter()
            .find(|element| element.id == 0x18538067)
            .expect("the segment is written");
        for cluster in mkwriter::reader::children(segment.body)
            .into_iter()
            .filter(|element| element.id == 0x1F43B675)
        {
            for group in mkwriter::reader::children(cluster.body)
                .into_iter()
                .filter(|element| element.id == 0xA0)
            {
                let mut track = None;
                let mut duration = None;
                for child in mkwriter::reader::children(group.body) {
                    match child.id {
                        0xA1 => track = Some(block_track(child.body)),
                        0x9B => duration = Some(mkwriter::reader::as_uint(child.body)),
                        _ => {}
                    }
                }
                if let (Some(track), Some(duration)) = (track, duration) {
                    out.push((track, duration));
                }
            }
        }
        out
    }

    #[test]
    fn a_live_writer_patches_video_block_durations_in_place() {
        // One declared track. The first packet of every announced track is taken before the
        // file starts, which is the opening-cluster rule of a live container.
        let plans = plans()[..1].to_vec();
        let (mut writer, target) = writer_and_target_in(&plans, usize::MAX, WriteMode::Live);
        writer.write_audio(audio_packet(0, 0)).expect("main at 0");
        writer
            .write_video(video_packet(0, true))
            .expect("video at 0");
        writer
            .write_video(video_packet(FRAME_NS, false))
            .expect("video at 20 ms");
        writer
            .write_video(video_packet(3 * FRAME_NS, false))
            .expect("video at 60 ms");

        // 40 ms of real interval between the first two video packets, then 20 ms after
        // Stop: the writer holds one offset and one PTS, never a payload.
        let before_stop = writer.stats();
        assert!(before_stop.live_block_durations);
        assert_eq!(
            before_stop.video_duration_patches, 2,
            "each later video packet patched the block before it"
        );
        assert!(
            before_stop.video_duration_pending,
            "the last video block waits for its real duration"
        );
        writer
            .finish_video(FRAME_NS)
            .expect("the Stop boundary closes the last video block");
        let stats = writer.stats();
        assert_eq!(stats.video_duration_patches, 3);
        assert!(!stats.video_duration_pending);
        assert_eq!(stats.video_packets, 3);
        writer.finish_audio(0, 0).expect("main finishes");
        writer.finalize().expect("the live container finalizes");

        // The video track is container track 1: every one of its blocks carries a real
        // duration, and the middle one carries the exact 40 ms interval to the next.
        let durations = block_durations(&target.bytes());
        let video: Vec<u64> = durations
            .iter()
            .filter(|(track, _)| *track == VIDEO_TRACK_NUMBER)
            .map(|(_, units)| *units)
            .collect();
        assert_eq!(
            video,
            vec![
                FRAME_NS / TIMESTAMP_SCALE_NS,
                40_000,
                FRAME_NS / TIMESTAMP_SCALE_NS
            ],
            "the live block durations are the real intervals, not placeholders: {durations:?}"
        );
    }

    #[test]
    fn live_duration_uses_shared_origin_and_the_explicit_stop_boundary() {
        let (mut writer, target) = writer_and_target_in(&plans()[..1], usize::MAX, WriteMode::Live);
        let origin = 5_000_000_350;
        writer.origin_ns = origin;
        let mut audio = audio_packet(0, 0);
        audio.pts.0 += origin;
        audio.dts.0 += origin;
        writer.write_audio(audio).unwrap();
        let mut video = video_packet(100_000_000, true);
        video.pts.0 += origin;
        video.dts.0 += origin;
        video.duration_ns = 200_000_000; // This estimate must not lengthen the file.
        writer.write_video(video).unwrap();
        writer.finish_video(10_000_000).unwrap();
        writer.finish_audio(0, 0).unwrap();
        let artifact = writer.finalize().unwrap();
        assert_eq!(artifact.duration_ms, 110.0);
        assert!(block_durations(&target.bytes()).contains(&(VIDEO_TRACK_NUMBER, 10_000)));

        let (mut unclosed, _) = writer_and_target_in(&plans()[..1], usize::MAX, WriteMode::Live);
        unclosed.write_audio(audio_packet(0, 0)).unwrap();
        unclosed.write_video(video_packet(0, true)).unwrap();
        unclosed.finish_audio(0, 0).unwrap();
        assert!(unclosed.finalize().is_err());
    }

    #[test]
    fn a_live_writer_never_invents_a_timestamp_for_an_earlier_video_packet() {
        let plans = plans()[..1].to_vec();
        let (mut writer, _target) = writer_and_target_in(&plans, usize::MAX, WriteMode::Live);
        writer.write_audio(audio_packet(0, 0)).expect("main at 0");
        writer
            .write_video(video_packet(0, true))
            .expect("video at 0");
        let error = writer
            .write_video(video_packet(0, false))
            .expect_err("a repeated video timestamp is refused");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(writer.failed());
        assert!(writer.finalize().is_err());
    }
}
