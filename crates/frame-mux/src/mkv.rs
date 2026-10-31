//! Single video channel Matroska writer.
//!
//! One H.264 Annex-B video track is written to a caller-chosen path on the calling
//! thread. The caller supplies the session's shared [`MediaTimestamp`] origin and the
//! encoder's [`EncodedStreamInfo`]; every packet timestamp is mapped as
//! `packet.pts - origin`, so a stream that does not start at the origin keeps its
//! real position and nothing is silently rebased. Variable frame intervals are
//! preserved: the configuration rate in [`EncodedStreamInfo`] never becomes a
//! container timestamp.
//!
//! Matroska stores timestamps in units of [`TIMESTAMP_SCALE_NS`], so every timestamp
//! and duration is rounded to the nearest unit with at most 500 ns of error. Rounding
//! may never reorder or collapse frames: a packet whose rounded timestamp is not
//! strictly later than the previous one is rejected.

use crate::ContainerWriter;
use crate::annexb::{
    access_unit_size, avc_decoder_configuration_record, length_prefixed_access_unit,
};
use crate::hevc;
mod backend;
mod errors;
mod output;
pub mod raster;
#[cfg(test)]
mod test_support;

pub mod av;
pub mod recovery;
use backend::Backend;
use errors::{
    REQUEST_ACTION, backend_error, backend_limit_error, output_path_error, request_error,
    storage_error, writer_error,
};
use frame_clock::MediaTimestamp;
use frame_contracts::{ErrorCode, RecorderError, RecordingArtifact, RecoveryStatus};
use frame_encode::{BitstreamFormat, EncodedPacket, EncodedStreamInfo, VideoColour};
use mkwriter::{Colour, Frame, Info, MasteringMetadata, TrackEntry, Tracks, VideoSettings};
use output::{OutputFile, OutputTarget, SinkAdapter};
use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};

/// Timestamps and durations are stored as nanoseconds divided by this scale, so a
/// container timestamp is rounded to the nearest microsecond.
pub const TIMESTAMP_SCALE_NS: u64 = 1_000;

/// Matroska number of the single video track. Matroska forbids track number zero.
pub const VIDEO_TRACK_NUMBER: u64 = 1;

/// Largest accepted packet, matching the encoder's declared maximum access unit.
pub const MAX_PACKET_BYTES: usize = 8 * 1024 * 1024;

/// Largest accepted codec configuration.
pub const MAX_CODEC_CONFIG_BYTES: usize = 64 * 1024;

/// Largest accepted number of keyframes, and therefore of cue points.
pub const MAX_KEYFRAMES: u64 = 65_536;

/// Ceiling for everything a writer retains: converted payload handed to the
/// backend, this crate's conversion scratch, the seek index and fixed file/header
/// scratch. Original input capacity is also charged during admission.
pub const MAX_RETAINED_BYTES: u64 = 64 * 1024 * 1024;

/// Track metadata admission derives from the existing 4 MiB recovery header budget.
pub const MAX_AUDIO_TRACKS: usize = (4 * 1024 * 1024) / 256;

/// Matroska number of the first audio track: audio plan index `i` is `i + 2`.
pub const FIRST_AUDIO_TRACK_NUMBER: u64 = VIDEO_TRACK_NUMBER + 1;

/// Fixed allowance for file/header buffers and streaming finalization scratch.
pub(super) const FINALIZE_SCRATCH_BYTES: u64 = 256 * 1024;

/// Codec identifier of an H.264 track in Matroska.
const AVC_CODEC_ID: &str = "V_MPEG4/ISO/AVC";
const HEVC_CODEC_ID: &str = "V_MPEGH/ISO/HEVC";
const AV1_CODEC_ID: &str = "V_AV1";

/// Stable track identifier. Output must not change between runs, so no random
/// segment or track identifier is used.
const VIDEO_TRACK_UID: u64 = 0x4652_414D_4500_0001;

pub(super) const WRITING_APP: &str = concat!("FREYM frame-mux ", env!("CARGO_PKG_VERSION"));

/// Validated facts taken from the encoder's stream description.
#[derive(Debug)]
pub(super) struct StreamPlan {
    pub(super) width: u64,
    pub(super) height: u64,
    pub(super) codec_id: &'static str,
    pub(super) codec_private: Vec<u8>,
    pub(super) bitstream_format: BitstreamFormat,
    pub(super) colour: Option<Colour>,
    pub(super) av1_sequence: Option<Vec<u8>>,
}

pub(super) fn video_settings(plan: &StreamPlan) -> VideoSettings {
    let mut settings = VideoSettings::new(plan.width, plan.height);
    settings.colour = plan.colour.clone();
    settings
}

pub(super) fn video_access_unit_size(
    format: BitstreamFormat,
    data: &[u8],
) -> Result<usize, String> {
    match format {
        BitstreamFormat::H264AnnexB => access_unit_size(data).map_err(|error| error.to_string()),
        BitstreamFormat::HevcAnnexB => {
            hevc::access_unit_size(data).map_err(|error| error.to_string())
        }
        BitstreamFormat::Av1Obu => Ok(data.len()),
    }
}

pub(super) fn video_access_unit(
    format: BitstreamFormat,
    data: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), String> {
    match format {
        BitstreamFormat::H264AnnexB => {
            length_prefixed_access_unit(data, out).map_err(|error| error.to_string())
        }
        BitstreamFormat::HevcAnnexB => {
            hevc::length_prefixed_access_unit(data, out).map_err(|error| error.to_string())
        }
        BitstreamFormat::Av1Obu => {
            out.clear();
            out.extend_from_slice(data);
            Ok(())
        }
    }
}

fn matroska_colour(source: VideoColour) -> Colour {
    Colour {
        matrix_coefficients: Some(u64::from(source.matrix_coefficients)),
        range: Some(u64::from(source.range)),
        transfer_characteristics: Some(u64::from(source.transfer_characteristics)),
        primaries: Some(u64::from(source.primaries)),
        bits_per_channel: Some(u64::from(source.bits_per_channel)),
        max_cll: source.content_light.map(|value| u64::from(value.max_cll)),
        max_fall: source.content_light.map(|value| u64::from(value.max_fall)),
        mastering: source.mastering.map(|value| MasteringMetadata {
            primary_r: Some(value.red),
            primary_g: Some(value.green),
            primary_b: Some(value.blue),
            white_point: Some(value.white),
            luminance_max: Some(value.luminance_max),
            luminance_min: Some(value.luminance_min),
        }),
        ..Colour::default()
    }
}

/// Counters and bounds of one writer, useful for diagnostics and evidence.
/// A complete-element prefix for which file flush and `sync_all` succeeded.
/// Pending live durations and final Opus trim can still change at finalization;
/// this is neither a completed recording nor a bound on power-loss data loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurableCheckpoint {
    pub sequence: u64,
    pub position_bytes: u64,
    pub container_end_ns: u64,
}

/// Counters and bounds of one writer, useful for diagnostics and evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MkvVideoWriterStats {
    pub packets: u64,
    pub keyframes: u64,
    pub raw_payload_bytes: u64,
    pub longest_packet_bytes: u64,
    /// First container timestamp, with the caller's origin already subtracted.
    pub first_pts_ns: Option<u64>,
    /// End of the last written packet on the container timeline.
    pub last_end_ns: u64,
    /// Conservative ceiling of everything currently retained, in bytes.
    pub retained_bytes_estimate: u64,
    pub last_checkpoint: Option<DurableCheckpoint>,
}

pub struct MkvVideoWriter {
    path: PathBuf,
    muxer: Option<Backend>,
    origin_ns: u64,
    accepted_track: Option<u32>,
    failure: Option<RecorderError>,
    packets: u64,
    keyframes: u64,
    raw_bytes: u64,
    longest_packet: u64,
    first_pts_units: Option<u64>,
    last_pts_units: Option<u64>,
    last_end_units: u64,
    scratch: Vec<u8>,
    bitstream_format: BitstreamFormat,
    av1_sequence: Option<Vec<u8>>,
}

impl std::fmt::Debug for MkvVideoWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MkvVideoWriter")
            .field("path", &self.path)
            .field("origin_ns", &self.origin_ns)
            .field("packets", &self.packets)
            .field("keyframes", &self.keyframes)
            .field("failed", &self.failed())
            .finish_non_exhaustive()
    }
}

impl MkvVideoWriter {
    /// Creates a new file. An existing path is never overwritten.
    pub fn create(
        path: &Path,
        stream: &EncodedStreamInfo,
        origin: MediaTimestamp,
    ) -> Result<Self, RecorderError> {
        let plan = plan_stream(stream)?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| output_path_error(path, &error))?;
        let target = Box::new(OutputFile::recording(file, path).map_err(|error| {
            storage_error(&error, "The output file could not be prepared for writing.")
        })?);
        Self::with_target(path.to_path_buf(), target, plan, origin)
    }

    pub(crate) fn create_segment(
        path: &Path,
        stream: &EncodedStreamInfo,
        origin: MediaTimestamp,
        lease: crate::session::SegmentLease,
    ) -> Result<Self, RecorderError> {
        let plan = plan_stream(stream)?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| output_path_error(path, &error))?;
        let target = Box::new(
            OutputFile::segment(file, path, lease)
                .map_err(|error| storage_error(&error, "The segment could not be prepared."))?,
        );
        Self::with_target(path.to_path_buf(), target, plan, origin)
    }

    fn with_target(
        path: PathBuf,
        target: Box<dyn OutputTarget>,
        plan: StreamPlan,
        origin: MediaTimestamp,
    ) -> Result<Self, RecorderError> {
        let info = container_info();
        let settings = video_settings(&plan);
        let track = TrackEntry::new(
            VIDEO_TRACK_NUMBER,
            VIDEO_TRACK_UID,
            mkwriter::ids::track_type::VIDEO,
            plan.codec_id,
        )
        .with_codec_private(plan.codec_private)
        .video(settings);
        let tracks = Tracks {
            entries: vec![track],
        };
        let muxer = Backend::new(SinkAdapter(target), &info, &tracks)
            .map_err(|error| backend_error(error, "The container header could not be written."))?;
        Ok(Self {
            path,
            muxer: Some(muxer),
            origin_ns: origin.0,
            accepted_track: None,
            failure: None,
            packets: 0,
            keyframes: 0,
            raw_bytes: 0,
            longest_packet: 0,
            first_pts_units: None,
            last_pts_units: None,
            last_end_units: 0,
            scratch: Vec::new(),
            bitstream_format: plan.bitstream_format,
            av1_sequence: plan.av1_sequence,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn stats(&self) -> MkvVideoWriterStats {
        MkvVideoWriterStats {
            packets: self.packets,
            keyframes: self.keyframes,
            raw_payload_bytes: self.raw_bytes,
            longest_packet_bytes: self.longest_packet,
            first_pts_ns: self.first_pts_units.map(|units| units * TIMESTAMP_SCALE_NS),
            last_end_ns: self.last_end_units * TIMESTAMP_SCALE_NS,
            retained_bytes_estimate: self.retained_bytes(),
            last_checkpoint: self.muxer.as_ref().and_then(Backend::last_checkpoint),
        }
    }

    /// Explicit lifecycle checkpoint. Normal writes checkpoint the first video
    /// block and then every five seconds while packets are arriving.
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

    pub(super) fn retained_bytes(&self) -> u64 {
        self.muxer.as_ref().map_or(0, Backend::retained_bytes)
            + self
                .av1_sequence
                .as_ref()
                .map_or(0, |bytes| bytes.capacity() as u64)
            + self.scratch.capacity() as u64
            + FINALIZE_SCRATCH_BYTES
    }

    pub(super) fn projected_retained_bytes(&self, input_capacity: usize, converted: usize) -> u64 {
        input_capacity as u64
            + self
                .av1_sequence
                .as_ref()
                .map_or(0, |bytes| bytes.capacity() as u64)
            + self.scratch.capacity().max(converted) as u64
            + self
                .muxer
                .as_ref()
                .map_or(0, |backend| backend.projected_bytes(converted))
            + FINALIZE_SCRATCH_BYTES
    }

    pub(super) fn fail<T>(&mut self, error: RecorderError) -> Result<T, RecorderError> {
        if self.failure.is_none() {
            self.failure = Some(error.clone());
        }
        Err(error)
    }

    fn write_packet(&mut self, packet: EncodedPacket) -> Result<(), RecorderError> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        if let Some(accepted) = self.accepted_track
            && accepted != packet.track_id
        {
            let error = request_error(
                format!(
                    "Packet track {} was offered to a writer that accepted track {accepted}; this writer owns exactly one video track.",
                    packet.track_id
                ),
                RecoveryStatus::Unknown,
            );
            return self.fail(error);
        }
        if packet.data.is_empty() {
            let error = request_error(
                "An empty packet cannot be written.".to_string(),
                RecoveryStatus::Unknown,
            );
            return self.fail(error);
        }
        if packet.data.len() > MAX_PACKET_BYTES {
            let error = request_error(
                format!(
                    "A packet of {} bytes exceeds the {MAX_PACKET_BYTES} byte limit.",
                    packet.data.len()
                ),
                RecoveryStatus::Unknown,
            );
            return self.fail(error);
        }
        if packet.dts != packet.pts {
            let error = request_error(
                format!(
                    "Packet DTS {} ns differs from PTS {} ns; this writer requires DTS equal to PTS.",
                    packet.dts.0, packet.pts.0
                ),
                RecoveryStatus::Unknown,
            );
            return self.fail(error);
        }
        let Some(offset) = packet.pts.0.checked_sub(self.origin_ns) else {
            let error = request_error(
                format!(
                    "Packet PTS {} ns is earlier than the session origin {} ns.",
                    packet.pts.0, self.origin_ns
                ),
                RecoveryStatus::Unknown,
            );
            return self.fail(error);
        };
        let pts_units = to_units(offset);
        let Some(container_pts_ns) = pts_units.checked_mul(TIMESTAMP_SCALE_NS) else {
            let error = request_error(
                format!("Packet PTS {offset} ns is beyond the container's representable range."),
                RecoveryStatus::Unknown,
            );
            return self.fail(error);
        };
        if let Some(previous) = self.last_pts_units
            && pts_units <= previous
        {
            let error = request_error(
                format!(
                    "Packet PTS {offset} ns maps to container unit {pts_units}, which is not later than the previous packet's {previous}; a container with {TIMESTAMP_SCALE_NS} ns units cannot hold two frames this close together."
                ),
                RecoveryStatus::Unknown,
            );
            return self.fail(error);
        }
        if packet.duration_ns == 0 {
            let error = request_error(
                "A packet with a zero duration cannot describe a frame.".to_string(),
                RecoveryStatus::Unknown,
            );
            return self.fail(error);
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
            return self.fail(error);
        }
        let Some(container_duration_ns) = duration_units.checked_mul(TIMESTAMP_SCALE_NS) else {
            let error = request_error(
                format!(
                    "A duration of {} ns is beyond the container's representable range.",
                    packet.duration_ns
                ),
                RecoveryStatus::Unknown,
            );
            return self.fail(error);
        };
        let Some(end_units) = pts_units
            .checked_add(duration_units)
            .filter(|end| end.checked_mul(TIMESTAMP_SCALE_NS).is_some())
        else {
            return self.fail(request_error(
                "Packet end exceeds the media timeline.",
                RecoveryStatus::Unknown,
            ));
        };
        if self.packets == 0 && !packet.keyframe {
            let error = request_error(
                "The first packet is not a keyframe, so the file would start without a random access point."
                    .to_string(),
                RecoveryStatus::Unknown,
            );
            return self.fail(error);
        }
        if packet.keyframe && self.keyframes + 1 > MAX_KEYFRAMES {
            let error = backend_limit_error(format!(
                "Refusing keyframe {} because this writer indexes at most {MAX_KEYFRAMES} of them.",
                self.keyframes + 1
            ));
            return self.fail(error);
        }

        // Scan before allocating; account for original allocation, conversion,
        // backend payload scratch and the bounded cue index together.
        if let Some(sequence) = self.av1_sequence.as_deref() {
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
        let converted = match video_access_unit_size(self.bitstream_format, &packet.data) {
            Ok(size) => size,
            Err(defect) => {
                return self.fail(request_error(
                    format!("The packet is not a valid Annex-B access unit: {defect}"),
                    RecoveryStatus::Unknown,
                ));
            }
        };
        if self.projected_retained_bytes(packet.data.capacity(), converted) > MAX_RETAINED_BYTES {
            return self.fail(backend_limit_error(
                "Packet would exceed the writer memory budget.",
            ));
        }
        let mut scratch = std::mem::take(&mut self.scratch);
        if let Err(defect) = video_access_unit(self.bitstream_format, &packet.data, &mut scratch) {
            self.scratch = scratch;
            return self.fail(request_error(
                format!("Invalid Annex-B: {defect}"),
                RecoveryStatus::Unknown,
            ));
        }
        if self.bitstream_format == BitstreamFormat::HevcAnnexB {
            match hevc::validate_recovered_access_unit(&scratch) {
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
        if packet.data.capacity() as u64
            + scratch.capacity() as u64
            + self
                .muxer
                .as_ref()
                .map_or(0, |backend| backend.projected_bytes(converted))
            + FINALIZE_SCRATCH_BYTES
            > MAX_RETAINED_BYTES
        {
            self.scratch = scratch;
            return self.fail(backend_limit_error(
                "Allocated conversion exceeds the writer memory budget.",
            ));
        }

        let frame = Frame {
            track: VIDEO_TRACK_NUMBER,
            timestamp_ns: container_pts_ns,
            data: &scratch,
            keyframe: packet.keyframe,
            duration_ns: Some(container_duration_ns),
            ..Frame::default()
        };
        let written = match self.muxer.as_mut() {
            Some(muxer) => muxer.write_frame(frame, false),
            None => Err(mkwriter::Error::Io(io::Error::other(
                "the writer was already finalized",
            ))),
        };
        self.scratch = scratch;
        if let Err(error) = written {
            return self.fail(backend_error(
                error,
                "The container rejected a packet that had passed validation.",
            ));
        }

        self.accepted_track = Some(packet.track_id);
        self.packets += 1;
        self.raw_bytes += packet.data.len() as u64;
        self.longest_packet = self.longest_packet.max(packet.data.len() as u64);
        self.first_pts_units.get_or_insert(pts_units);
        self.last_pts_units = Some(pts_units);
        self.last_end_units = self.last_end_units.max(end_units);
        if packet.keyframe {
            self.keyframes += 1;
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

    fn finish(mut self) -> Result<RecordingArtifact, RecorderError> {
        if let Some(error) = self.failure.take() {
            // The container is already incomplete. Dropping it without finalizing
            // keeps the partial file for diagnosis and never reports an artifact.
            self.muxer = None;
            return Err(error);
        }
        let Some(muxer) = self.muxer.take() else {
            return Err(request_error(
                "This writer was already finalized.".to_string(),
                RecoveryStatus::Unknown,
            ));
        };
        if self.packets == 0 {
            return Err(request_error(
                format!(
                    "{} holds no packets, so it is not a recording.",
                    self.path.display()
                ),
                RecoveryStatus::Unknown,
            ));
        }
        let adapter = match muxer.finalize(&[VIDEO_TRACK_NUMBER], None) {
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

        // Container duration is measured from the shared origin, including a
        // legitimate gap before this track's first frame.
        let duration_ns = self.last_end_units * TIMESTAMP_SCALE_NS;
        Ok(RecordingArtifact {
            path: self.path.to_string_lossy().into_owned(),
            duration_ms: duration_ns as f64 / 1_000_000.0,
            audio_tracks: 0,
        })
    }
}

impl ContainerWriter<EncodedPacket> for MkvVideoWriter {
    fn write(&mut self, packet: EncodedPacket) -> Result<(), RecorderError> {
        self.write_packet(packet)
    }

    fn finalize(self) -> Result<RecordingArtifact, RecorderError> {
        self.finish()
    }
}

/// The header facts every Matroska file from this crate declares.
pub(super) fn container_info() -> Info {
    Info {
        timestamp_scale: TIMESTAMP_SCALE_NS,
        muxing_app: WRITING_APP.to_string(),
        writing_app: WRITING_APP.to_string(),
        segment_uuid: None,
        date_utc: None,
        duration: None,
    }
}

/// Rounds nanoseconds to the nearest container unit, so the error is at most half a
/// unit and never overflows.
pub(super) fn to_units(ns: u64) -> u64 {
    let units = ns / TIMESTAMP_SCALE_NS;
    if (ns % TIMESTAMP_SCALE_NS) * 2 >= TIMESTAMP_SCALE_NS {
        units + 1
    } else {
        units
    }
}

pub(super) fn plan_stream(stream: &EncodedStreamInfo) -> Result<StreamPlan, RecorderError> {
    if stream.allows_reordering {
        return Err(request_error(
            "A stream that allows reordering cannot be written: this writer stores DTS equal to PTS and does not reorder frames."
                .to_string(),
            RecoveryStatus::NoRecording,
        ));
    }
    if stream.codec_config.is_empty() {
        return Err(writer_error(
            ErrorCode::UnsupportedCodec,
            RecoveryStatus::NoRecording,
            "The stream description carries no codec configuration.".to_string(),
            REQUEST_ACTION,
        ));
    }
    if stream.codec_config.len() > MAX_CODEC_CONFIG_BYTES {
        return Err(request_error(
            format!(
                "A codec configuration of {} bytes exceeds the {MAX_CODEC_CONFIG_BYTES} byte limit.",
                stream.codec_config.len()
            ),
            RecoveryStatus::NoRecording,
        ));
    }
    let (codec_id, codec_private) = match stream.bitstream_format {
        BitstreamFormat::H264AnnexB => (
            AVC_CODEC_ID,
            avc_decoder_configuration_record(&stream.codec_config).map_err(|defect| {
                writer_error(
                    ErrorCode::UnsupportedCodec,
                    RecoveryStatus::NoRecording,
                    format!("The codec configuration is not usable Matroska AVC: {defect}"),
                    REQUEST_ACTION,
                )
            })?,
        ),
        BitstreamFormat::HevcAnnexB => (
            HEVC_CODEC_ID,
            hevc::decoder_configuration_record(&stream.codec_config).map_err(|defect| {
                writer_error(
                    ErrorCode::UnsupportedCodec,
                    RecoveryStatus::NoRecording,
                    format!("The codec configuration is not usable Matroska HEVC: {defect}"),
                    REQUEST_ACTION,
                )
            })?,
        ),
        BitstreamFormat::Av1Obu => {
            let (private, sequence) = crate::av1::configuration(&stream.codec_config)
                .map_err(|error| request_error(error, RecoveryStatus::NoRecording))?;
            if sequence.width != stream.width.get() || sequence.height != stream.height.get() {
                return Err(request_error(
                    "AV1 sequence geometry disagrees with its stream description",
                    RecoveryStatus::NoRecording,
                ));
            }
            if let Some(colour) = stream.colour {
                if (
                    colour.bits_per_channel,
                    colour.primaries,
                    colour.transfer_characteristics,
                    colour.matrix_coefficients,
                    colour.range,
                ) != (
                    sequence.depth,
                    sequence.primaries,
                    sequence.transfer,
                    sequence.matrix,
                    sequence.range,
                ) {
                    return Err(request_error(
                        format!(
                            "AV1 colour/depth metadata {colour:?} disagrees with its coded sequence {sequence:?}"
                        ),
                        RecoveryStatus::NoRecording,
                    ));
                }
            } else if sequence.depth != 8 {
                return Err(request_error(
                    "AV1 Main10 requires source colour metadata",
                    RecoveryStatus::NoRecording,
                ));
            }
            (AV1_CODEC_ID, private)
        }
    };
    if stream.bitstream_format == BitstreamFormat::HevcAnnexB
        && stream.colour.is_none_or(|colour| {
            colour.bits_per_channel != 8 + (codec_private[17] & 7)
                || colour.bits_per_channel != 8 + (codec_private[18] & 7)
        })
    {
        return Err(request_error(
            "HEVC requires source colour metadata matching its coded bit depth.".to_string(),
            RecoveryStatus::NoRecording,
        ));
    }
    if let Some(colour) = stream.colour {
        colour.validate()?;
    }
    Ok(StreamPlan {
        width: u64::from(stream.width.get()),
        height: u64::from(stream.height.get()),
        codec_id,
        codec_private,
        bitstream_format: stream.bitstream_format,
        colour: stream.colour.map(matroska_colour),
        av1_sequence: if stream.bitstream_format == BitstreamFormat::Av1Obu {
            Some(
                crate::av1::sequence_payload(&stream.codec_config)
                    .map_err(|error| request_error(error, RecoveryStatus::NoRecording))?
                    .to_vec(),
            )
        } else {
            None
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mkv::test_support::FailingTarget;
    use std::num::NonZeroU32;

    const SPS: [u8; 8] = [0x67, 0x64, 0x00, 0x3C, 0xAC, 0x2B, 0x40, 0x14];
    const PPS: [u8; 4] = [0x68, 0xEE, 0x3C, 0xB0];

    pub(super) fn stream_info() -> EncodedStreamInfo {
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

    fn keyframe() -> Vec<u8> {
        vec![0, 0, 0, 1, 0x65, 0x11, 0x22, 0x33]
    }

    fn delta() -> Vec<u8> {
        vec![0, 0, 0, 1, 0x41, 0x44, 0x55, 0x00, 0x00]
    }

    fn writer_with_target(limit: usize, origin_ns: u64) -> MkvVideoWriter {
        let plan = plan_stream(&stream_info()).expect("valid stream");
        MkvVideoWriter::with_target(
            PathBuf::from("injected.mkv"),
            Box::new(FailingTarget::new(limit)),
            plan,
            MediaTimestamp(origin_ns),
        )
        .expect("the header fits the injected target")
    }

    fn packet(pts_ns: u64, duration_ns: u64, keyframe: bool, data: Vec<u8>) -> EncodedPacket {
        EncodedPacket {
            track_id: 7,
            pts: MediaTimestamp(pts_ns),
            dts: MediaTimestamp(pts_ns),
            duration_ns,
            keyframe,
            data,
        }
    }

    #[test]
    fn a_failed_write_is_sticky_and_stops_finalization() {
        let mut writer = writer_with_target(4096, 0);
        writer
            .write(packet(0, 16_683_334, true, keyframe()))
            .expect("the first packet fits");
        let error = writer
            .write(packet(
                16_683_334,
                16_683_334,
                false,
                [0, 0, 0, 1, 0x41, 0x00, 0x00, 0x00].repeat(2048),
            ))
            .expect_err("the injected target refuses this write");
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(writer.failed());
        assert_eq!(writer.stats().packets, 1);
        let repeated = writer
            .write(packet(33_366_668, 16_683_334, false, delta()))
            .expect_err("the writer stays failed");
        assert_eq!(repeated.message, error.message);
        let artifact = writer.finalize().expect_err("no artifact after a failure");
        assert_eq!(artifact.message, error.message);
    }

    #[test]
    fn a_writer_without_packets_never_finalizes_into_an_artifact() {
        let writer = writer_with_target(1_000_000, 0);
        assert!(!writer.failed());
        let error = writer
            .finalize()
            .expect_err("an empty writer is not a recording");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert_eq!(error.recovery, RecoveryStatus::Unknown);
    }

    #[test]
    fn a_rejected_packet_prevents_finalization() {
        let mut writer = writer_with_target(1_000_000, 0);
        writer
            .write(packet(0, 16_683_334, true, keyframe()))
            .expect("a valid first packet");
        let error = writer
            .write(packet(16_683_334, 16_683_334, false, Vec::new()))
            .expect_err("empty packets are rejected");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(writer.finalize().is_err());
    }

    #[test]
    fn packet_scratch_is_reused_without_accumulating_packet_history() {
        let mut writer = writer_with_target(usize::MAX, 0);
        let mut data = vec![0x55; 16_384];
        data[..5].copy_from_slice(&[0, 0, 0, 1, 0x65]);
        writer.write(packet(0, 1000, true, data.clone())).unwrap();
        let retained = writer.retained_bytes();
        data[4] = 0x41;
        for index in 1..100 {
            writer
                .write(packet(index * 1000, 1000, false, data.clone()))
                .unwrap();
        }
        assert_eq!(writer.retained_bytes(), retained);
        assert!(retained < MAX_RETAINED_BYTES);
    }

    #[test]
    fn packet_end_overflow_is_rejected_before_backend_write() {
        let mut writer = writer_with_target(usize::MAX, 0);
        let error = writer
            .write(packet(u64::MAX - 1615, 2000, true, keyframe()))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert_eq!(writer.stats().packets, 0);
        assert!(writer.finalize().is_err());
    }

    #[test]
    fn rounding_uses_the_nearest_unit_within_half_a_unit() {
        assert_eq!(to_units(0), 0);
        assert_eq!(to_units(499), 0);
        assert_eq!(to_units(500), 1);
        assert_eq!(to_units(1_499), 1);
        assert_eq!(to_units(1_500), 2);
        assert_eq!(to_units(160_836_650), 160_837);
        assert_eq!(to_units(14_507_800), 14_508);
        assert_eq!(to_units(u64::MAX), 18_446_744_073_709_552);
    }

    #[test]
    fn a_backwards_timestamp_is_the_first_stored_failure() {
        let mut writer = writer_with_target(1_000_000, 0);
        writer
            .write(packet(1_000, 1_000, true, keyframe()))
            .expect("a valid first packet");
        let first = writer
            .write(packet(999, 1_000, false, delta()))
            .expect_err("a backwards timestamp is rejected");
        assert_eq!(first.code, ErrorCode::InvalidRequest);
        let second = writer
            .write(packet(2_000, 1_000, false, delta()))
            .expect_err("the first failure is preserved");
        assert_eq!(second.message, first.message);
    }

    #[test]
    fn dts_differing_from_pts_is_rejected() {
        let mut writer = writer_with_target(1_000_000, 0);
        let mut reordered = packet(0, 1_000, true, keyframe());
        reordered.dts = MediaTimestamp(500);
        let error = writer
            .write(reordered)
            .expect_err("reordering is not supported");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(error.message.contains("DTS"));
    }

    #[test]
    fn a_different_track_is_rejected_after_the_first_packet() {
        let mut writer = writer_with_target(1_000_000, 0);
        writer
            .write(packet(0, 1_000, true, keyframe()))
            .expect("a valid first packet");
        let mut other = packet(2_000, 1_000, false, delta());
        other.track_id = 9;
        let error = writer.write(other).expect_err("a second track is rejected");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(error.message.contains("track 9"));
    }

    #[test]
    fn empty_oversized_and_non_keyframe_first_packets_are_rejected() {
        let mut writer = writer_with_target(usize::MAX, 0);
        let error = writer
            .write(packet(0, 1_000, true, Vec::new()))
            .expect_err("empty");
        assert!(error.message.contains("empty"));
        let mut writer = writer_with_target(usize::MAX, 0);
        let error = writer
            .write(packet(0, 1_000, true, vec![0u8; MAX_PACKET_BYTES + 1]))
            .expect_err("oversized");
        assert!(error.message.contains("exceeds"));
        let mut writer = writer_with_target(usize::MAX, 0);
        let error = writer
            .write(packet(0, 1_000, false, delta()))
            .expect_err("not a keyframe");
        assert!(error.message.contains("keyframe"));
    }

    #[test]
    fn a_packet_that_is_not_annex_b_is_rejected() {
        let mut writer = writer_with_target(usize::MAX, 0);
        let error = writer
            .write(packet(0, 1_000, true, vec![0x41, 0x11, 0x22]))
            .expect_err("a buffer without start codes is not an access unit");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(error.message.contains("Annex-B"));
    }

    #[test]
    fn stream_plan_rejects_reordering_and_unusable_configuration() {
        let mut reordering = stream_info();
        reordering.allows_reordering = true;
        assert_eq!(
            plan_stream(&reordering)
                .expect_err("reordering is unsupported")
                .code,
            ErrorCode::InvalidRequest
        );
        let mut without_config = stream_info();
        without_config.codec_config.clear();
        assert_eq!(
            plan_stream(&without_config)
                .expect_err("no configuration")
                .code,
            ErrorCode::UnsupportedCodec
        );
        let mut oversized = stream_info();
        oversized.codec_config = vec![0u8; MAX_CODEC_CONFIG_BYTES + 1];
        assert_eq!(
            plan_stream(&oversized).expect_err("too large").code,
            ErrorCode::InvalidRequest
        );
        let mut slices_only = stream_info();
        slices_only.codec_config = vec![0, 0, 0, 1, 0x65, 0x11];
        assert_eq!(
            plan_stream(&slices_only)
                .expect_err("no parameter sets")
                .code,
            ErrorCode::UnsupportedCodec
        );
    }

    #[test]
    fn a_non_zero_origin_keeps_the_real_start_of_the_stream() {
        let mut writer = writer_with_target(usize::MAX, 1_000_000);
        writer
            .write(packet(4_000_000, 500_000, true, keyframe()))
            .expect("the first packet is after the origin");
        let stats = writer.stats();
        assert_eq!(stats.first_pts_ns, Some(3_000_000));
        assert_eq!(stats.last_end_ns, 3_500_000);
        let mut completed = writer_with_target(usize::MAX, 1_000_000);
        completed
            .write(packet(4_000_000, 500_000, true, keyframe()))
            .unwrap();
        assert_eq!(completed.finalize().unwrap().duration_ms, 3.5);
        assert_eq!(
            writer
                .write(packet(999_999, 1_000, false, delta()))
                .expect_err("earlier than the origin")
                .code,
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn rounding_rejects_intervals_the_container_cannot_hold() {
        let mut writer = writer_with_target(usize::MAX, 0);
        writer
            .write(packet(0, 1_000, true, keyframe()))
            .expect("a valid first packet");
        let error = writer
            .write(packet(400, 1_000, false, delta()))
            .expect_err("a 400 ns interval rounds onto the previous frame");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(error.message.contains("cannot hold two frames"));

        let mut writer = writer_with_target(usize::MAX, 0);
        let error = writer
            .write(packet(0, 1, true, keyframe()))
            .expect_err("a sub-unit duration is refused");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(error.message.contains("container unit"));
    }

    #[test]
    fn a_cue_limit_is_reported_before_the_container_grows_without_bound() {
        let mut writer = writer_with_target(usize::MAX, 0);
        writer.keyframes = MAX_KEYFRAMES;
        let error = writer
            .write(packet(0, 1_000, true, keyframe()))
            .expect_err("the index limit is enforced");
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(error.message.contains("indexes at most"));
    }
}
