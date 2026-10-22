// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Bounded recovery of the complete prefix of an unfinished Matroska recording.
//!
//! [`recover`] reads a container this crate's writer produced, keeps every complete
//! element and block before the first incomplete or unusable one, and writes a new,
//! playable Matroska file with a fresh cluster stream, cue index, seek head and
//! duration. The source file is opened read-only and is never modified, truncated or
//! renamed, and the output path is created exclusively, so an existing file is never
//! overwritten.
//!
//! # What recovery does and does not claim
//!
//! Recovery never re-encodes. Every block payload is copied byte for byte, the
//! container's timestamp scale and variable frame timestamps are preserved, and each
//! block's own `BlockDuration`, `ReferenceBlock` and `DiscardPadding` fields are
//! re-emitted unchanged. The `Tracks` element is validated field by field and then
//! re-emitted verbatim, so track numbers, `TrackUID`s, names, `CodecPrivate`
//! (`OpusHead` pre-skip, AVC configuration record), `CodecDelay` and `SeekPreRoll` are
//! carried over exactly as found.
//!
//! A successful return means the recovered prefix is complete and playable. It never
//! means the original recording finished: the bytes after the first incomplete element
//! are not parsed, so the number of packets they hold is unknown, and the encoder's
//! final audio trim is only known when the source itself was finalized. Both facts are
//! reported in [`RecoverySummary`] and are never guessed.
//!
//! # Bounds
//!
//! The source is streamed; recovered media is never accumulated in memory. Working
//! memory is one block buffer, the `Tracks` element, the bounded cue index and fixed
//! finalization scratch, and it is checked against [`MAX_RECOVERY_WORKING_BYTES`]
//! before every allocation. Element nesting is bounded by
//! [`MAX_RECOVERY_ELEMENT_DEPTH`]: the parser descends only into the structures this
//! profile defines and never follows an unknown master.

use super::errors::{
    backend_error, backend_limit_error, output_path_error, storage_error, writer_error,
};
use super::output::{OutputFile, SinkAdapter};
use super::{
    FINALIZE_SCRATCH_BYTES, MAX_KEYFRAMES, MAX_PACKET_BYTES, TIMESTAMP_SCALE_NS, WRITING_APP,
    to_units,
};
use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus};
use mkwriter::sink::{MasterScope, Sink};
use mkwriter::{EbmlWriter, ElementId, ebml, ids};
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Ceiling for everything recovery retains while it runs.
pub const MAX_RECOVERY_WORKING_BYTES: u64 = 64 * 1024 * 1024;

/// Deepest element nesting recovery descends into. The profile it recovers needs five:
/// `Segment`, `Tracks`/`Cluster`, `TrackEntry`/`BlockGroup`, `Video`/`Audio` and their
/// leaf fields. No unknown master is ever entered, so the parser cannot go deeper.
pub const MAX_RECOVERY_ELEMENT_DEPTH: usize = 8;

const _: () = assert!(
    MAX_RECOVERY_ELEMENT_DEPTH >= 5,
    "the recovered profile nests five elements deep"
);

/// Largest `Tracks` element accepted, far above the writer's own track ceiling.
const MAX_TRACKS_BYTES: u64 = 4 * 1024 * 1024;

/// Largest `Info` element accepted.
const MAX_INFO_BYTES: u64 = 1024 * 1024;

/// Largest `SimpleBlock` or `Block` element body: one maximum access unit plus header.
const MAX_BLOCK_BYTES: u64 = (MAX_PACKET_BYTES + 64 * 1024) as u64;

/// Largest `BlockGroup` body: one block plus its duration, reference and padding.
const MAX_BLOCK_GROUP_BYTES: u64 = (MAX_PACKET_BYTES + 128 * 1024) as u64;

/// Fixed `SeekHead` reservation, the same width the writer reserves.
const SEEK_RESERVATION_BYTES: usize = 256;

/// Matroska forbids track number zero; this is the ceiling the profile stays inside.
const MAX_TRACK_NUMBER: u64 = 65_535;

/// Codec identifiers of the profile this recovery accepts.
const AVC_CODEC_ID: &str = "V_MPEG4/ISO/AVC";
const HEVC_CODEC_ID: &str = "V_MPEGH/ISO/HEVC";
const AV1_CODEC_ID: &str = "V_AV1";
const OPUS_CODEC_ID: &str = "A_OPUS";

/// `OpusHead` version 1 with no channel mapping table.
const OPUS_HEAD_MIN_BYTES: usize = 19;
const OPUS_SAMPLE_RATE: u64 = 48_000;

const RECOVERY_ACTION: &str = "Point recovery at a Matroska file this crate's writer produced, and never publish a recovered prefix as a completed recording.";

macro_rules! element_ids {
    ($($name:ident => $element:ident),* $(,)?) => {
        $(const $name: u32 = ids::$element.as_u32();)*
    };
}

element_ids! {
    ID_EBML => EBML,
    ID_SEGMENT => SEGMENT,
    ID_SEEK_HEAD => SEEK_HEAD,
    ID_INFO => INFO,
    ID_TRACKS => TRACKS,
    ID_CLUSTER => CLUSTER,
    ID_CUES => CUES,
    ID_TAGS => TAGS,
    ID_ATTACHMENTS => ATTACHMENTS,
    ID_CHAPTERS => CHAPTERS,
    ID_VOID => VOID,
    ID_CRC32 => CRC_32,
    ID_TIMESTAMP => TIMESTAMP,
    ID_SIMPLE_BLOCK => SIMPLE_BLOCK,
    ID_BLOCK_GROUP => BLOCK_GROUP,
    ID_BLOCK => BLOCK,
    ID_BLOCK_DURATION => BLOCK_DURATION,
    ID_REFERENCE_BLOCK => REFERENCE_BLOCK,
    ID_DISCARD_PADDING => DISCARD_PADDING,
    ID_POSITION => POSITION,
    ID_PREV_SIZE => PREV_SIZE,
    ID_ENCRYPTED_BLOCK => ENCRYPTED_BLOCK,
    ID_TRACK_ENTRY => TRACK_ENTRY,
    ID_TRACK_NUMBER => TRACK_NUMBER,
    ID_TRACK_TYPE => TRACK_TYPE,
    ID_CODEC_ID => CODEC_ID,
    ID_CODEC_PRIVATE => CODEC_PRIVATE,
    ID_CODEC_DELAY => CODEC_DELAY,
    ID_SEEK_PRE_ROLL => SEEK_PRE_ROLL,
    ID_CONTENT_ENCODINGS => CONTENT_ENCODINGS,
    ID_NAME => NAME,
    ID_VIDEO => VIDEO,
    ID_AUDIO => AUDIO,
    ID_PIXEL_WIDTH => PIXEL_WIDTH,
    ID_PIXEL_HEIGHT => PIXEL_HEIGHT,
    ID_CHANNELS => CHANNELS,
    ID_SAMPLING_FREQUENCY => SAMPLING_FREQUENCY,
    ID_DOC_TYPE => DOC_TYPE,
    ID_DOC_TYPE_READ_VERSION => DOC_TYPE_READ_VERSION,
    ID_TIMESTAMP_SCALE => TIMESTAMP_SCALE,
    ID_MUXING_APP => MUXING_APP,
    ID_WRITING_APP => WRITING_APP,
    ID_DURATION => DURATION,
}

const TRACK_TYPE_VIDEO: u64 = ids::track_type::VIDEO;
const TRACK_TYPE_AUDIO: u64 = ids::track_type::AUDIO;

/// Which stream a recovered track carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveredTrackKind {
    Video,
    Audio,
}

impl RecoveredTrackKind {
    /// Stable label for reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
        }
    }
}

/// One declared track, with everything the container declared about it and what was
/// actually recovered from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredTrack {
    pub number: u64,
    pub kind: RecoveredTrackKind,
    pub codec_id: String,
    pub name: Option<String>,
    /// `CodecDelay` in nanoseconds, exactly as the track declared it.
    pub codec_delay_ns: Option<u64>,
    /// `SeekPreRoll` in nanoseconds, exactly as the track declared it.
    pub seek_pre_roll_ns: Option<u64>,
    pub codec_private_bytes: usize,
    /// `OpusHead` pre-skip in 48 kHz samples per channel, when the track is Opus.
    pub opus_pre_skip_samples: Option<u16>,
    pub packets: u64,
    pub keyframes: u64,
    /// Container end of the last recovered block of this track.
    pub last_end_ns: u64,
    /// `DiscardPadding` of the last recovered block, as the source stored it.
    pub last_discard_padding_ns: u64,
}

/// An element recovery deliberately did not carry into the output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedElement {
    pub name: &'static str,
    pub id: u32,
    pub bytes: u64,
    pub count: u64,
}

/// Why recovery stopped before the end of the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailKind {
    /// Recovery covered every byte the segment declares.
    None,
    /// The source ends inside an element or block.
    Truncated,
    /// The bytes are present but are not the container this profile describes.
    Corrupt,
    /// The source could no longer be read.
    Unreadable,
}

impl TailKind {
    /// Stable label for reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Truncated => "truncated",
            Self::Corrupt => "corrupt",
            Self::Unreadable => "unreadable",
        }
    }
}

/// The bytes recovery refused to interpret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownTail {
    /// Absolute offset of the first incomplete or unusable element.
    pub start_offset: u64,
    pub bytes: u64,
    pub kind: TailKind,
    /// The concrete reason recovery stopped, or `None` when nothing was left over.
    pub reason: Option<String>,
}

/// What one recovery run found. Every byte count is measured, never estimated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverySummary {
    pub source: String,
    pub output: String,
    pub source_bytes: u64,
    pub output_bytes: u64,
    /// Source bytes up to the first incomplete element: the covered prefix.
    pub recovered_bytes: u64,
    /// Source bytes after that point, which recovery never parsed.
    pub dropped_incomplete_bytes: u64,
    /// Always `None`: the tail is not parsed, so its packet count is unknown.
    pub dropped_incomplete_packets: Option<u64>,
    pub unknown_tail: UnknownTail,
    pub recovered_packets: u64,
    pub recovered_video_packets: u64,
    pub recovered_audio_packets: u64,
    /// Recovered video keyframes, which is also the cue point count.
    pub recovered_keyframes: u64,
    pub cues: u64,
    pub clusters: u64,
    /// Timestamp scale carried over from the source, in nanoseconds per unit.
    pub timestamp_scale_ns: u64,
    /// Container end of everything recovered, including untrimmed final packets.
    pub container_end_ns: u64,
    /// The duration written into the output's `Info` element.
    pub duration_ns: u64,
    /// `Duration` the source declared, when it declared one.
    pub source_declared_duration_ns: Option<u64>,
    /// True only when the source's own segment size, index and end all agree.
    pub source_finalized: bool,
    /// True when the encoder's final audio trim is known, which needs a finalized source.
    pub final_trim_known: bool,
    pub tracks: Vec<RecoveredTrack>,
    pub skipped_elements: Vec<SkippedElement>,
    pub notes: Vec<String>,
    /// Conservative ceiling of everything recovery retained, in bytes.
    pub working_bytes_estimate: u64,
}

impl RecoverySummary {
    /// True when the recovered prefix holds a random access point a player can use.
    pub fn is_playable_prefix(&self) -> bool {
        self.recovered_keyframes > 0 && self.recovered_video_packets > 0
    }
}

/// Recovers the complete prefix of `source` into the new file `output`.
///
/// Fails without producing an output file when the source is not the AVC/HEVC/Opus
/// Matroska profile this crate writes, when its track metadata is unusable, or when no
/// complete video keyframe could be recovered. An existing `output` is never
/// overwritten and the source is never written to.
pub fn recover(source: &Path, output: &Path) -> Result<RecoverySummary, RecorderError> {
    let mut state = Recovery::open(source, output)?;
    state.run()?;
    state.finish()
}

/// Session recovery additionally binds the source's exact track header to the
/// durable manifest. Keep the existing parser and standalone recovery behavior.
pub(crate) fn recover_segment(
    source: &Path,
    output: &Path,
    tracks_ebml: &[u8],
) -> Result<RecoverySummary, RecorderError> {
    let mut state = Recovery::open(source, output)?;
    state.expected_tracks = Some(tracks_ebml.to_vec());
    state.run()?;
    state.finish()
}

/// Why a bounded read, parse or write stopped.
enum Halt {
    /// The source ends inside this element; everything before it is complete.
    Incomplete(String),
    /// The bytes are present but are not the container this profile describes.
    Corrupt(String),
    /// The rest of the source could not be read.
    Unreadable(io::Error),
    /// No artifact is possible.
    Fatal(RecorderError),
}

impl Halt {
    fn incomplete(message: impl Into<String>) -> Self {
        Self::Incomplete(message.into())
    }

    fn corrupt(message: impl Into<String>) -> Self {
        Self::Corrupt(message.into())
    }

    fn reason(&self) -> String {
        match self {
            Self::Incomplete(message) | Self::Corrupt(message) => message.clone(),
            Self::Unreadable(error) => format!("the source could not be read: {error}"),
            Self::Fatal(error) => error.message.clone(),
        }
    }

    fn kind(&self) -> TailKind {
        match self {
            Self::Incomplete(_) => TailKind::Truncated,
            Self::Corrupt(_) | Self::Fatal(_) => TailKind::Corrupt,
            Self::Unreadable(_) => TailKind::Unreadable,
        }
    }
}

/// A reader over the source file with checked, position-tracked reads.
///
/// Every read is bounded by the file's real length, so a truncated source is reported
/// rather than panicking and no element header can index past the end of the file.
struct Source {
    file: BufReader<File>,
    position: u64,
    length: u64,
}

impl Source {
    fn open(path: &Path) -> Result<Self, RecorderError> {
        let file = File::open(path).map_err(|error| {
            storage_error(
                &error,
                "The source recording could not be opened for reading.",
            )
        })?;
        let length = file
            .metadata()
            .map_err(|error| {
                storage_error(&error, "The source recording's length could not be read.")
            })?
            .len();
        Ok(Self {
            file: BufReader::new(file),
            position: 0,
            length,
        })
    }

    fn position(&self) -> u64 {
        self.position
    }

    fn length(&self) -> u64 {
        self.length
    }

    fn remaining(&self) -> u64 {
        self.length.saturating_sub(self.position)
    }

    fn seek_to(&mut self, offset: u64) -> Result<(), Halt> {
        if offset > self.length {
            return Err(Halt::incomplete(format!(
                "byte {offset} is past the end of the {} byte source",
                self.length
            )));
        }
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(Halt::Unreadable)?;
        self.position = offset;
        Ok(())
    }

    /// Skips `bytes` without reading them.
    fn skip(&mut self, bytes: u64) -> Result<(), Halt> {
        let Some(end) = self.position.checked_add(bytes) else {
            return Err(Halt::corrupt("an element size overflows the file offset"));
        };
        self.seek_to(end)
    }

    fn read_exact_into(&mut self, buf: &mut [u8]) -> Result<(), Halt> {
        if self.remaining() < buf.len() as u64 {
            return Err(Halt::incomplete(format!(
                "only {} of the {} bytes at byte {} remain",
                self.remaining(),
                buf.len(),
                self.position
            )));
        }
        self.file.read_exact(buf).map_err(Halt::Unreadable)?;
        self.position += buf.len() as u64;
        Ok(())
    }

    fn read_byte(&mut self) -> Result<u8, Halt> {
        let mut byte = [0u8; 1];
        self.read_exact_into(&mut byte)?;
        Ok(byte[0])
    }

    /// Reads an element header at the current position. `Ok(None)` means the source
    /// ends exactly at an element boundary.
    fn read_header(&mut self) -> Result<Option<Header>, Halt> {
        if self.remaining() == 0 {
            return Ok(None);
        }
        let start = self.position;
        let first = self.read_byte()?;
        let id_length = first.leading_zeros() as usize + 1;
        if id_length > 4 {
            return Err(Halt::corrupt(format!(
                "byte {start} does not begin a valid EBML element identifier"
            )));
        }
        let mut id = u32::from(first);
        for _ in 1..id_length {
            id = (id << 8) | u32::from(self.read_byte()?);
        }
        let first = self.read_byte()?;
        let size_length = first.leading_zeros() as usize + 1;
        if size_length > 8 {
            return Err(Halt::corrupt(format!(
                "the element at byte {start} declares an invalid size width"
            )));
        }
        let mask: u8 = if size_length >= 8 {
            0
        } else {
            0xFF >> size_length
        };
        let mut value = u64::from(first & mask);
        for _ in 1..size_length {
            value = (value << 8) | u64::from(self.read_byte()?);
        }
        let data_bits = 7 * size_length as u32;
        let all_ones = if data_bits >= 64 {
            u64::MAX
        } else {
            (1u64 << data_bits) - 1
        };
        Ok(Some(Header {
            id,
            start,
            body_start: self.position,
            size: (value != all_ones).then_some(value),
        }))
    }

    /// Reads exactly `size` bytes into `buf`, which is cleared first.
    fn read_body_into(&mut self, size: u64, buf: &mut Vec<u8>) -> Result<(), Halt> {
        let length = usize::try_from(size)
            .map_err(|_| Halt::corrupt("an element body does not fit this platform"))?;
        buf.clear();
        if buf.capacity() < length {
            buf.reserve_exact(length);
        }
        buf.resize(length, 0);
        self.read_exact_into(buf)
    }
}

/// One parsed element header.
#[derive(Debug, Clone, Copy)]
struct Header {
    id: u32,
    /// Offset of the identifier's first byte.
    start: u64,
    /// Offset of the body's first byte.
    body_start: u64,
    /// Declared body length, or `None` for an unknown size.
    size: Option<u64>,
}

/// A bounds-checked cursor over an element body already held in memory.
struct Slice<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Slice<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn read_vint(&mut self) -> Result<u64, String> {
        let Some(&first) = self.data.get(self.position) else {
            return Err("a variable-length integer is cut short".to_string());
        };
        let length = first.leading_zeros() as usize + 1;
        if length > 8 {
            return Err("a variable-length integer declares an invalid width".to_string());
        }
        let mask: u8 = if length >= 8 { 0 } else { 0xFF >> length };
        let mut value = u64::from(first & mask);
        for index in 1..length {
            let byte = *self
                .data
                .get(self.position + index)
                .ok_or_else(|| "a variable-length integer is cut short".to_string())?;
            value = (value << 8) | u64::from(byte);
        }
        self.position += length;
        Ok(value)
    }

    /// Reads the next element header, returning its id and the body's byte range.
    fn read_header(&mut self) -> Result<Option<(u32, std::ops::Range<usize>)>, String> {
        if self.position == self.data.len() {
            return Ok(None);
        }
        let start = self.position;
        let first = self.data[start];
        let id_length = first.leading_zeros() as usize + 1;
        if id_length > 4 || start + id_length > self.data.len() {
            return Err(format!(
                "byte {start} does not begin a valid element identifier"
            ));
        }
        let mut id = u32::from(first);
        for index in 1..id_length {
            id = (id << 8) | u32::from(self.data[start + index]);
        }
        self.position += id_length;
        let first = *self
            .data
            .get(self.position)
            .ok_or_else(|| format!("the element at byte {start} has no size"))?;
        let size_length = first.leading_zeros() as usize + 1;
        if size_length > 8 {
            return Err(format!(
                "the element at byte {start} declares an invalid size width"
            ));
        }
        let mask: u8 = if size_length >= 8 {
            0
        } else {
            0xFF >> size_length
        };
        let mut value = u64::from(first & mask);
        for index in 1..size_length {
            let byte = *self
                .data
                .get(self.position + index)
                .ok_or_else(|| format!("the element at byte {start} has no size"))?;
            value = (value << 8) | u64::from(byte);
        }
        let data_bits = 7 * size_length as u32;
        let all_ones = if data_bits >= 64 {
            u64::MAX
        } else {
            (1u64 << data_bits) - 1
        };
        if value == all_ones {
            return Err(format!(
                "the element at byte {start} declares an unknown size, which this body cannot hold"
            ));
        }
        self.position += size_length;
        let length = usize::try_from(value)
            .map_err(|_| format!("the element at byte {start} is too large"))?;
        let end = self
            .position
            .checked_add(length)
            .filter(|end| *end <= self.data.len())
            .ok_or_else(|| {
                format!(
                    "the element at byte {start} declares {length} bytes but only {} remain",
                    self.data.len() - self.position
                )
            })?;
        let range = self.position..end;
        self.position = end;
        Ok(Some((id, range)))
    }
}

/// A small integer field, kept byte for byte so it can be re-emitted unchanged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RawField {
    bytes: [u8; 8],
    length: u8,
}

impl RawField {
    fn new(body: &[u8]) -> Result<Self, String> {
        if body.is_empty() || body.len() > 8 {
            return Err(format!("a {}-byte integer field", body.len()));
        }
        let mut bytes = [0u8; 8];
        bytes[..body.len()].copy_from_slice(body);
        Ok(Self {
            bytes,
            length: body.len() as u8,
        })
    }

    fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.length as usize]
    }

    fn is_present(&self) -> bool {
        self.length > 0
    }
}

/// The `BlockGroup` fields other than the block itself, kept verbatim.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct GroupExtras {
    block_duration: RawField,
    reference_block: RawField,
    discard_padding: RawField,
}

/// One complete block, parsed but not yet written.
#[derive(Debug)]
struct ParsedBlock<'a> {
    track: u64,
    /// The timestamp the block stated, relative to its cluster.
    relative: i16,
    flags: u8,
    /// True for a `SimpleBlock`, false for a `Block` inside a `BlockGroup`.
    simple: bool,
    /// Whether the container marks this block as a random access point.
    keyframe: bool,
    duration_units: Option<u64>,
    discard_padding_ns: Option<i64>,
    extras: GroupExtras,
    payload: &'a [u8],
}

/// A track as the source declared it, plus what recovery took from it.
#[derive(Debug, Clone)]
struct TrackState {
    number: u64,
    kind: RecoveredTrackKind,
    codec_id: String,
    name: Option<String>,
    codec_private_bytes: usize,
    av1_sequence: Option<Vec<u8>>,
    opus_pre_skip_samples: Option<u16>,
    codec_delay_ns: Option<u64>,
    seek_pre_roll_ns: Option<u64>,
    last_pts_units: Option<u64>,
    packets: u64,
    keyframes: u64,
    last_end_ns: u64,
    last_discard_padding_ns: u64,
}

impl TrackState {
    fn summary(&self) -> RecoveredTrack {
        RecoveredTrack {
            number: self.number,
            kind: self.kind,
            codec_id: self.codec_id.clone(),
            name: self.name.clone(),
            codec_delay_ns: self.codec_delay_ns,
            seek_pre_roll_ns: self.seek_pre_roll_ns,
            codec_private_bytes: self.codec_private_bytes,
            opus_pre_skip_samples: self.opus_pre_skip_samples,
            packets: self.packets,
            keyframes: self.keyframes,
            last_end_ns: self.last_end_ns,
            last_discard_padding_ns: self.last_discard_padding_ns,
        }
    }
}

/// The `Info` facts recovery carries into the output.
#[derive(Debug, Clone)]
struct InfoFacts {
    timestamp_scale_ns: u64,
    muxing_app: String,
    writing_app: String,
    declared_duration_ns: Option<u64>,
}

/// Validated source metadata, needed before the output can be opened.
#[derive(Debug, Clone)]
struct Metadata {
    info: InfoFacts,
    tracks_body: Vec<u8>,
    tracks: Vec<TrackState>,
    video_track: u64,
}

/// One indexed random access point, exactly as the writer's own backend records it.
#[derive(Debug, Clone, Copy)]
struct Cue {
    time: u64,
    cluster: u64,
    relative: u64,
    track: u64,
}

/// One cluster the output has open.
struct OpenCluster {
    scope: MasterScope,
    base: u64,
    position: u64,
    body_start: u64,
}

/// The new file recovery writes.
struct Output {
    out: EbmlWriter<SinkAdapter>,
    path: PathBuf,
    segment: MasterScope,
    seek_start: u64,
    seek_bytes: usize,
    info_position: u64,
    tracks_position: u64,
    duration_offset: u64,
    video_track: u64,
    cluster: Option<OpenCluster>,
    cues: Vec<Cue>,
    clusters: u64,
    payload: Vec<u8>,
}

impl Output {
    fn create(path: &Path, metadata: &Metadata) -> Result<Self, RecorderError> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| output_path_error(path, &error))?;
        let target = Box::new(OutputFile::new(file).map_err(|error| {
            storage_error(
                &error,
                "The recovered file could not be prepared for writing.",
            )
        })?);
        let mut out = EbmlWriter::new(SinkAdapter(target));
        mkwriter::write_ebml_header(&mut out, "matroska", 4, 2).map_err(|error| {
            container_error(
                error,
                "The recovered container's header could not be written.",
            )
        })?;
        let segment = begin_master(&mut out, ids::SEGMENT)?;
        let seek_start = out.position();
        out.write_void(SEEK_RESERVATION_BYTES)
            .map_err(|error| container_error(error, "The seek head could not be reserved."))?;
        let seek_bytes = (out.position() - seek_start) as usize;
        let info_position = out.position() - segment.body_start();
        let info = begin_master(&mut out, ids::INFO)?;
        out.write_uint(ids::TIMESTAMP_SCALE, metadata.info.timestamp_scale_ns)
            .map_err(|error| container_error(error, "The timestamp scale could not be written."))?;
        out.write_utf8(ids::MUXING_APP, &metadata.info.muxing_app)
            .map_err(|error| {
                container_error(error, "The muxing application could not be written.")
            })?;
        out.write_utf8(ids::WRITING_APP, &metadata.info.writing_app)
            .map_err(|error| {
                container_error(error, "The writing application could not be written.")
            })?;
        out.write_float(ids::DURATION, 1.0)
            .map_err(|error| container_error(error, "The duration field could not be reserved."))?;
        let duration_offset = out.position() - 8;
        end_master(&mut out, info)?;
        let tracks_position = out.position() - segment.body_start();
        write_element(&mut out, ID_TRACKS, &metadata.tracks_body)?;
        Ok(Self {
            out,
            path: path.to_path_buf(),
            segment,
            seek_start,
            seek_bytes,
            info_position,
            tracks_position,
            duration_offset,
            video_track: metadata.video_track,
            cluster: None,
            cues: Vec::new(),
            clusters: 0,
            payload: Vec::new(),
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// Writes one complete block, opening a new cluster when the relative timestamp
    /// would leave the 16-bit range. The payload and the block's own metadata elements
    /// are copied byte for byte; only the relative timestamp is recomputed.
    fn write_block(&mut self, units: u64, block: &ParsedBlock<'_>) -> Result<(), RecorderError> {
        self.roll_cluster(units)?;
        let cluster = self.cluster.as_ref().expect("an open cluster");
        let relative = i16::try_from(i128::from(units) - i128::from(cluster.base))
            .expect("the cluster rolls over before the relative timestamp leaves i16");
        let relative_position = self.out.position() - cluster.body_start;
        let cluster_position = cluster.position;

        let mut payload = std::mem::take(&mut self.payload);
        payload.clear();
        if payload.capacity() < block.payload.len() + 12 {
            payload.reserve_exact(block.payload.len() + 12);
        }
        ebml::write_vint(&mut payload, block.track).map_err(|error| {
            container_error(
                error,
                "A recovered block's track number could not be written.",
            )
        })?;
        payload.extend_from_slice(&relative.to_be_bytes());
        payload.push(block.flags);
        payload.extend_from_slice(block.payload);
        let result = if block.simple {
            write_element(&mut self.out, ID_SIMPLE_BLOCK, &payload)
        } else {
            write_block_group(&mut self.out, &payload, &block.extras)
        };
        self.payload = payload;
        result?;

        if block.keyframe && block.track == self.video_track {
            self.cues.push(Cue {
                time: units,
                cluster: cluster_position,
                relative: relative_position,
                track: block.track,
            });
        }
        Ok(())
    }

    fn roll_cluster(&mut self, units: u64) -> Result<(), RecorderError> {
        let needed = self.cluster.as_ref().is_none_or(|cluster| {
            i16::try_from(i128::from(units) - i128::from(cluster.base)).is_err()
        });
        if !needed {
            return Ok(());
        }
        self.close_cluster()?;
        let position = self.out.position() - self.segment.body_start();
        let scope = begin_master(&mut self.out, ids::CLUSTER)?;
        // A cue's relative position is measured from the cluster's own body start,
        // which is where the first block could begin, not from its timestamp.
        let body_start = scope.body_start();
        self.out
            .write_uint(ids::TIMESTAMP, units)
            .map_err(|error| container_error(error, "A cluster timestamp could not be written."))?;
        self.cluster = Some(OpenCluster {
            scope,
            base: units,
            position,
            body_start,
        });
        self.clusters += 1;
        Ok(())
    }

    fn close_cluster(&mut self) -> Result<(), RecorderError> {
        if let Some(cluster) = self.cluster.take() {
            end_master(&mut self.out, cluster.scope)?;
        }
        Ok(())
    }

    /// Closes the last cluster, writes the new cue index and seek head, patches the
    /// duration, then flushes, syncs and closes the file.
    fn finish(mut self, duration_units: u64) -> Result<u64, RecorderError> {
        self.close_cluster()?;
        let cues_position = self.out.position() - self.segment.body_start();
        let cues = begin_master(&mut self.out, ids::CUES)?;
        for cue in &self.cues {
            self.out
                .write_master(ids::CUE_POINT, |writer| {
                    writer.write_uint(ids::CUE_TIME, cue.time)?;
                    writer.write_master(ids::CUE_TRACK_POSITIONS, |track| {
                        track.write_uint(ids::CUE_TRACK, cue.track)?;
                        track.write_uint(ids::CUE_CLUSTER_POSITION, cue.cluster)?;
                        track.write_uint(ids::CUE_RELATIVE_POSITION, cue.relative)
                    })
                })
                .map_err(|error| container_error(error, "A cue point could not be written."))?;
        }
        end_master(&mut self.out, cues)?;

        let mut seek = mkwriter::seekhead::SeekHead::new();
        seek.add(ids::INFO, self.info_position);
        seek.add(ids::TRACKS, self.tracks_position);
        seek.add(ids::CUES, cues_position);
        let mut patch = seek
            .to_bytes()
            .map_err(|error| container_error(error, "The seek head could not be encoded."))?;
        let remainder = self.seek_bytes.checked_sub(patch.len()).ok_or_else(|| {
            container_error(
                mkwriter::Error::Io(io::Error::other("seek header reservation exhausted")),
                "The seek head does not fit its reservation.",
            )
        })?;
        if remainder >= 2 {
            let width = (1..=8)
                .find(|width| {
                    remainder > *width
                        && ((remainder - 1 - *width) as u64) < (1u64 << (7 * width)) - 1
                })
                .expect("small fixed seek reservation");
            ebml::write_id(&mut patch, ids::VOID.as_u32()).map_err(|error| {
                container_error(error, "The seek padding could not be encoded.")
            })?;
            ebml::write_vint_with_len(&mut patch, (remainder - 1 - width) as u64, width).map_err(
                |error| container_error(error, "The seek padding could not be encoded."),
            )?;
            patch.resize(self.seek_bytes, 0);
        } else if remainder != 0 {
            return Err(container_error(
                mkwriter::Error::Io(io::Error::other("invalid seek padding")),
                "The seek head reservation cannot be padded.",
            ));
        }
        patch_at(&mut self.out, self.seek_start, &patch)?;
        patch_at(
            &mut self.out,
            self.duration_offset,
            &(duration_units as f64).to_be_bytes(),
        )?;
        let segment = self.segment;
        end_master(&mut self.out, segment)?;
        self.out
            .get_mut()
            .flush()
            .map_err(|error| storage_error(&error, "The recovered file could not be flushed."))?;
        let SinkAdapter(target) = self.out.into_inner();
        let bytes = target.position();
        target.finish().map_err(|error| {
            storage_error(
                &error,
                "The recovered file could not be flushed, synced and closed.",
            )
        })?;
        Ok(bytes)
    }
}

/// Recovery state for one source file.
struct Recovery {
    source: Source,
    source_path: PathBuf,
    output_path: PathBuf,
    output: Option<Output>,
    metadata: Option<Metadata>,
    expected_tracks: Option<Vec<u8>>,
    info: Option<InfoFacts>,
    segment_end: u64,
    declared_segment_size: bool,
    cues_seen: bool,
    current_element_start: u64,
    stop_offset: u64,
    stop_reason: Option<String>,
    stop_kind: TailKind,
    packets: u64,
    video_packets: u64,
    audio_packets: u64,
    keyframes: u64,
    first_video_is_keyframe: Option<bool>,
    end_units: u64,
    skipped: Vec<SkippedElement>,
    notes: Vec<String>,
    payload: Vec<u8>,
}

impl Recovery {
    fn open(source_path: &Path, output_path: &Path) -> Result<Self, RecorderError> {
        let source = Source::open(source_path)?;
        if source.length() == 0 {
            return Err(source_error("The source recording is empty."));
        }
        Ok(Self {
            source,
            source_path: source_path.to_path_buf(),
            output_path: output_path.to_path_buf(),
            output: None,
            metadata: None,
            expected_tracks: None,
            info: None,
            segment_end: 0,
            declared_segment_size: false,
            cues_seen: false,
            current_element_start: 0,
            stop_offset: 0,
            stop_reason: None,
            stop_kind: TailKind::None,
            packets: 0,
            video_packets: 0,
            audio_packets: 0,
            keyframes: 0,
            first_video_is_keyframe: None,
            end_units: 0,
            skipped: Vec::new(),
            notes: Vec::new(),
            payload: Vec::new(),
        })
    }

    /// Walks the source. Only a failure that makes an artifact impossible returns an
    /// error; every other stop keeps the complete prefix and is reported afterwards.
    fn run(&mut self) -> Result<(), RecorderError> {
        let walk = self
            .read_ebml_header()
            .and_then(|()| self.read_segment_header())
            .and_then(|()| self.walk_segment());
        match walk {
            Ok(()) => {
                self.stop_offset = self.segment_end;
                if self.source.length() > self.segment_end {
                    self.stop_reason = Some(format!(
                        "{} bytes follow the declared end of the segment",
                        self.source.length() - self.segment_end
                    ));
                }
                Ok(())
            }
            Err(Halt::Fatal(error)) => {
                self.discard_output();
                Err(error)
            }
            Err(halt) => {
                self.stop_offset = self.current_element_start.min(self.segment_end);
                self.stop_kind = halt.kind();
                self.stop_reason = Some(halt.reason());
                Ok(())
            }
        }
    }

    fn finish(mut self) -> Result<RecoverySummary, RecorderError> {
        let working = self.working_bytes();
        let Some(metadata) = self.metadata.take() else {
            let reason = self
                .stop_reason
                .clone()
                .unwrap_or_else(|| "the source holds no usable tracks element".to_string());
            return Err(source_error(format!(
                "{} holds no recoverable Matroska recording: {reason}",
                self.source_path.display()
            )));
        };
        if self.output.is_none() {
            let reason = self
                .stop_reason
                .clone()
                .unwrap_or_else(|| "the source holds no cluster".to_string());
            return Err(source_error(format!(
                "{} holds no complete cluster before the first unusable element: {reason}",
                self.source_path.display()
            )));
        }
        if self.first_video_is_keyframe == Some(false) {
            self.discard_output();
            return Err(source_error(format!(
                "{} starts with a video block that is not a keyframe, so the recovered file would hold no random access point at its start.",
                self.source_path.display()
            )));
        }
        if self.keyframes == 0 || self.video_packets == 0 {
            let reason = self
                .stop_reason
                .clone()
                .unwrap_or_else(|| "the source ends before any complete video block".to_string());
            self.discard_output();
            return Err(source_error(format!(
                "{} holds no complete video keyframe to recover: {reason}",
                self.source_path.display()
            )));
        }

        let scale = metadata.info.timestamp_scale_ns;
        let container_end_ns = self.end_units.saturating_mul(scale);
        let source_finalized = self.declared_segment_size
            && self.cues_seen
            && self.stop_reason.is_none()
            && self.segment_end == self.source.length();
        let audio_complete = metadata
            .tracks
            .iter()
            .filter(|track| track.kind == RecoveredTrackKind::Audio)
            .all(|track| track.packets > 0);
        let final_trim_known = source_finalized && audio_complete;
        let duration_units = if final_trim_known {
            trimmed_end_units(&metadata)
        } else {
            self.end_units
        };
        if !final_trim_known {
            self.notes.push(
                "the source was not finalized, so the encoder's final audio trim is unknown; the recovered duration is the untrimmed container end of the recovered blocks and is not a claim about the recording's exact length".to_string(),
            );
        }
        let declared = metadata.info.declared_duration_ns;
        if let (Some(declared), true) = (declared, final_trim_known) {
            let recovered = duration_units.saturating_mul(scale);
            if declared != recovered {
                self.notes.push(format!(
                    "the source declared {declared} ns of duration and the recovered timeline ends at {recovered} ns"
                ));
            }
        }

        let output = self.output.take().expect("a created output");
        let output_path = output.path().to_path_buf();
        let cues = output.cues.len() as u64;
        let clusters = output.clusters;
        let output_bytes = match output.finish(duration_units) {
            Ok(bytes) => bytes,
            Err(error) => {
                discard_file(&output_path);
                return Err(error);
            }
        };
        self.notes.push(
            "the recovered file restores the complete prefix only; a successful recovery does not mean the original recording finished".to_string(),
        );

        let recovered_bytes = self.stop_offset.min(self.segment_end);
        Ok(RecoverySummary {
            source: self.source_path.to_string_lossy().into_owned(),
            output: output_path.to_string_lossy().into_owned(),
            source_bytes: self.source.length(),
            output_bytes,
            recovered_bytes,
            dropped_incomplete_bytes: self.source.length().saturating_sub(recovered_bytes),
            dropped_incomplete_packets: None,
            unknown_tail: UnknownTail {
                start_offset: self.stop_offset,
                bytes: self.source.length().saturating_sub(self.stop_offset),
                kind: self.stop_kind,
                reason: self.stop_reason.clone(),
            },
            recovered_packets: self.packets,
            recovered_video_packets: self.video_packets,
            recovered_audio_packets: self.audio_packets,
            recovered_keyframes: self.keyframes,
            cues,
            clusters,
            timestamp_scale_ns: scale,
            container_end_ns,
            duration_ns: duration_units.saturating_mul(scale),
            source_declared_duration_ns: declared,
            source_finalized,
            final_trim_known,
            tracks: metadata.tracks.iter().map(TrackState::summary).collect(),
            skipped_elements: self.skipped.clone(),
            notes: std::mem::take(&mut self.notes),
            working_bytes_estimate: working,
        })
    }

    /// Removes only a file this run created exclusively.
    fn discard_output(&mut self) {
        if let Some(output) = self.output.take() {
            let path = output.path().to_path_buf();
            drop(output);
            discard_file(&path);
        }
    }

    /// Conservative ceiling of everything recovery holds in memory.
    fn working_bytes(&self) -> u64 {
        // Reserve every bounded copy at its maximum: input block, rewritten block,
        // BlockGroup scratch and metadata parsing/output. Cue Vec growth is at most
        // twice its capped element count. This also holds while payload is moved out.
        3 * (MAX_BLOCK_GROUP_BYTES + 128)
            + super::MAX_CODEC_CONFIG_BYTES as u64 // One frozen AV1 sequence copy.
            + 4 * MAX_TRACKS_BYTES
            + 2 * MAX_INFO_BYTES
            + 2 * MAX_KEYFRAMES * std::mem::size_of::<Cue>() as u64
            + FINALIZE_SCRATCH_BYTES
    }

    fn check_budget(&mut self, incoming: u64) -> Result<(), Halt> {
        let projected = self.working_bytes();
        if incoming > MAX_BLOCK_GROUP_BYTES || projected > MAX_RECOVERY_WORKING_BYTES {
            return Err(Halt::Fatal(backend_limit_error(format!(
                "Recovery would exceed its {MAX_RECOVERY_WORKING_BYTES} byte working budget."
            ))));
        }
        Ok(())
    }

    fn read_ebml_header(&mut self) -> Result<(), Halt> {
        self.current_element_start = self.source.position();
        let header = self
            .source
            .read_header()?
            .ok_or_else(|| Halt::corrupt("the source holds no EBML header"))?;
        if header.id != ID_EBML {
            return Err(Halt::corrupt(format!(
                "the source starts with a 0x{:X} element instead of an EBML header",
                header.id
            )));
        }
        let size = self.bounded_size(&header, MAX_INFO_BYTES)?;
        let mut body = Vec::new();
        self.source.read_body_into(size, &mut body)?;
        let mut slice = Slice::new(&body);
        let mut doc_type: Option<String> = None;
        let mut read_version: Option<u64> = None;
        while let Some((id, range)) = slice.read_header().map_err(Halt::corrupt)? {
            let field = &body[range];
            match id {
                ID_DOC_TYPE => doc_type = Some(utf8(field).map_err(Halt::corrupt)?),
                ID_DOC_TYPE_READ_VERSION => {
                    read_version = Some(uint(field).map_err(Halt::corrupt)?);
                }
                _ => {}
            }
        }
        let doc_type =
            doc_type.ok_or_else(|| Halt::corrupt("the EBML header declares no DocType"))?;
        if doc_type != "matroska" {
            return Err(Halt::corrupt(format!(
                "the source declares the {doc_type} DocType, not matroska"
            )));
        }
        if let Some(read_version) = read_version
            && read_version > 4
        {
            return Err(Halt::corrupt(format!(
                "the source needs DocTypeReadVersion {read_version}, which this recovery does not implement"
            )));
        }
        Ok(())
    }

    fn read_segment_header(&mut self) -> Result<(), Halt> {
        self.current_element_start = self.source.position();
        let header = self
            .source
            .read_header()?
            .ok_or_else(|| Halt::corrupt("the source holds no segment"))?;
        if header.id != ID_SEGMENT {
            return Err(Halt::corrupt(format!(
                "the source holds a 0x{:X} element where its segment belongs",
                header.id
            )));
        }
        self.segment_end = match header.size {
            None => {
                self.notes.push(
                    "the segment declares an unknown size, so it is read to the end of the file"
                        .to_string(),
                );
                self.source.length()
            }
            Some(size) => {
                let declared_end = header.body_start.saturating_add(size);
                if size == 0 || declared_end > self.source.length() {
                    self.notes.push(format!(
                        "the segment declares {size} bytes, which does not fit the {} byte source; it was never finalized and is read to the end of the file",
                        self.source.length()
                    ));
                    self.source.length()
                } else {
                    self.declared_segment_size = true;
                    if declared_end < self.source.length() {
                        self.notes.push(format!(
                            "{} bytes follow the declared end of the segment and are not part of it",
                            self.source.length() - declared_end
                        ));
                    }
                    declared_end
                }
            }
        };
        Ok(())
    }

    fn walk_segment(&mut self) -> Result<(), Halt> {
        loop {
            if self.source.position() >= self.segment_end {
                return Ok(());
            }
            self.current_element_start = self.source.position();
            let Some(header) = self.source.read_header()? else {
                return Ok(());
            };
            if header.body_start > self.segment_end {
                return Err(Halt::incomplete(format!(
                    "the element header at byte {} extends past the declared segment end",
                    header.start
                )));
            }
            let declared_end = match header.size {
                Some(size) => Some(header.body_start.checked_add(size).ok_or_else(|| {
                    Halt::corrupt("an element size overflows the segment offset")
                })?),
                None => None,
            };
            // A cluster whose declared body does not fit is still read element by
            // element: each of its complete children is validated on its own, so the
            // blocks before the first incomplete one are preserved instead of being
            // dropped with the cluster they sit in. Every other element is only
            // trusted when its whole body is present.
            if let Some(end) = declared_end
                && end > self.segment_end
                && header.id != ID_CLUSTER
            {
                return Err(Halt::incomplete(format!(
                    "the {} element at byte {} declares {} bytes but only {} remain",
                    element_name(header.id),
                    header.start,
                    end - header.body_start,
                    self.segment_end - header.body_start
                )));
            }
            match header.id {
                ID_SEEK_HEAD | ID_VOID | ID_CRC32 | ID_CUES => {
                    if header.id == ID_CUES {
                        self.cues_seen = true;
                    }
                    self.skip_element(&header)?;
                }
                ID_INFO => {
                    let facts = self.read_info(&header)?;
                    if self.metadata.is_some() || self.info.is_some() {
                        return Err(Halt::corrupt(
                            "a second Info element cannot change the recovered timeline",
                        ));
                    }
                    self.info = Some(facts);
                }
                ID_TRACKS => {
                    let (tracks, body) = self.read_tracks(&header)?;
                    let info = self.info.take().ok_or_else(|| {
                        Halt::Fatal(source_error(format!(
                            "{} declares its tracks before any Info element, so its timeline is unknown.",
                            self.source_path.display()
                        )))
                    })?;
                    let video_track = single_video_track(&tracks).map_err(|defect| {
                        Halt::Fatal(source_error(format!(
                            "{} declares no usable video track: {defect}",
                            self.source_path.display()
                        )))
                    })?;
                    self.metadata = Some(Metadata {
                        info,
                        tracks_body: body,
                        tracks,
                        video_track,
                    });
                }
                ID_CLUSTER => {
                    if self.metadata.is_none() {
                        return Err(Halt::corrupt(format!(
                            "the cluster at byte {} appears before any usable Tracks element",
                            header.start
                        )));
                    }
                    if self.output.is_none() {
                        let metadata = self.metadata.as_ref().expect("checked metadata");
                        let output =
                            Output::create(&self.output_path, metadata).map_err(Halt::Fatal)?;
                        self.output = Some(output);
                    }
                    // Only a cluster whose declared body is present is bounded by it.
                    // A streamed master reserves a zero size until closure. Its
                    // body starts here but has no trustworthy end yet.
                    let cluster_end = declared_end.filter(|end| {
                        header.size.is_some_and(|size| size > 0) && *end <= self.segment_end
                    });
                    if cluster_end.is_none() && header.size.is_some_and(|size| size > 0) {
                        self.notes.push(format!(
                            "the cluster at byte {} declares a body that does not fit the source, so it is read as a truncated cluster and every complete block inside it is recovered",
                            header.start
                        ));
                    }
                    self.read_cluster(&header, cluster_end)?;
                    // Preserve complete source cluster boundaries, including
                    // clusters closed for a durable checkpoint. The next block
                    // starts a fresh output cluster even at the same timestamp.
                    self.output
                        .as_mut()
                        .expect("opened output")
                        .close_cluster()
                        .map_err(Halt::Fatal)?;
                }
                ID_TAGS | ID_ATTACHMENTS | ID_CHAPTERS => {
                    return Err(Halt::corrupt(format!(
                        "the segment holds a {} element at byte {}, which this writer's profile never produces",
                        element_name(header.id),
                        header.start
                    )));
                }
                other => {
                    return Err(Halt::corrupt(format!(
                        "the segment holds an unexpected {} element (0x{other:X}) at byte {}",
                        element_name(other),
                        header.start
                    )));
                }
            }
        }
    }

    fn skip_element(&mut self, header: &Header) -> Result<(), Halt> {
        let size = header.size.ok_or_else(|| {
            Halt::corrupt(format!(
                "the {} element at byte {} declares an unknown size, so its end cannot be trusted",
                element_name(header.id),
                header.start
            ))
        })?;
        self.source.skip(size)?;
        let name = element_name(header.id);
        match self.skipped.iter_mut().find(|entry| entry.id == header.id) {
            Some(entry) => {
                entry.bytes += size;
                entry.count += 1;
            }
            None => self.skipped.push(SkippedElement {
                name,
                id: header.id,
                bytes: size,
                count: 1,
            }),
        }
        Ok(())
    }

    fn read_info(&mut self, header: &Header) -> Result<InfoFacts, Halt> {
        let size = self.bounded_size(header, MAX_INFO_BYTES)?;
        self.check_budget(size)?;
        let mut body = Vec::new();
        self.source.read_body_into(size, &mut body)?;
        let mut slice = Slice::new(&body);
        let mut scale = None;
        let mut muxing_app = None;
        let mut writing_app = None;
        let mut duration: Option<&[u8]> = None;
        while let Some((id, range)) = slice.read_header().map_err(Halt::corrupt)? {
            let field = &body[range];
            match id {
                ID_TIMESTAMP_SCALE => scale = Some(uint(field).map_err(Halt::corrupt)?),
                ID_MUXING_APP => muxing_app = Some(utf8(field).map_err(Halt::corrupt)?),
                ID_WRITING_APP => writing_app = Some(utf8(field).map_err(Halt::corrupt)?),
                ID_DURATION => duration = Some(field),
                _ => {}
            }
        }
        let timestamp_scale_ns = scale.ok_or_else(|| {
            Halt::Fatal(source_error(format!(
                "{} declares no TimestampScale, so its timeline cannot be interpreted.",
                self.source_path.display()
            )))
        })?;
        if timestamp_scale_ns == 0 || timestamp_scale_ns > 1_000_000_000 {
            return Err(Halt::Fatal(source_error(format!(
                "{} declares a TimestampScale of {timestamp_scale_ns} ns, which no recording uses.",
                self.source_path.display()
            ))));
        }
        if timestamp_scale_ns != TIMESTAMP_SCALE_NS {
            self.notes.push(format!(
                "the source stores {timestamp_scale_ns} ns per timestamp unit instead of the writer's {TIMESTAMP_SCALE_NS} ns; the scale was carried over unchanged"
            ));
        }
        let declared_duration_ns = match duration {
            Some(field) => Some(scale_duration(field, timestamp_scale_ns).map_err(|defect| {
                Halt::Fatal(source_error(format!(
                    "{} declares an unusable duration: {defect}",
                    self.source_path.display()
                )))
            })?),
            None => None,
        };
        Ok(InfoFacts {
            timestamp_scale_ns,
            muxing_app: muxing_app.unwrap_or_else(|| WRITING_APP.to_string()),
            writing_app: writing_app.unwrap_or_else(|| WRITING_APP.to_string()),
            declared_duration_ns,
        })
    }

    fn read_tracks(&mut self, header: &Header) -> Result<(Vec<TrackState>, Vec<u8>), Halt> {
        let size = self.bounded_size(header, MAX_TRACKS_BYTES)?;
        self.check_budget(size)?;
        let mut body = Vec::new();
        self.source.read_body_into(size, &mut body)?;
        if let Some(expected) = &self.expected_tracks {
            let encoded_size = header.body_start - header.start + size;
            if encoded_size != expected.len() as u64 {
                return Err(Halt::Fatal(source_error(
                    "Segment track header does not match its durable manifest.",
                )));
            }
            self.source.seek_to(header.start)?;
            let mut encoded = Vec::new();
            self.source.read_body_into(encoded_size, &mut encoded)?;
            if &encoded != expected {
                return Err(Halt::Fatal(source_error(
                    "Segment track header does not match its durable manifest.",
                )));
            }
        }
        let tracks = parse_tracks(&body).map_err(|defect| {
            Halt::Fatal(source_error(format!(
                "{} declares track metadata this recovery cannot use: {defect}",
                self.source_path.display()
            )))
        })?;
        Ok((tracks, body))
    }

    fn bounded_size(&self, header: &Header, limit: u64) -> Result<u64, Halt> {
        let size = header.size.ok_or_else(|| {
            Halt::corrupt(format!(
                "the {} element at byte {} declares an unknown size",
                element_name(header.id),
                header.start
            ))
        })?;
        if size > limit {
            return Err(Halt::corrupt(format!(
                "the {} element at byte {} declares {size} bytes, above the {limit} byte limit for its kind",
                element_name(header.id),
                header.start
            )));
        }
        Ok(size)
    }

    /// Reads one cluster. `declared_end` bounds the cluster only when its whole
    /// declared body is present.
    ///
    /// Two situations leave a cluster without a trustworthy end, and both are read as
    /// an unterminated cluster whose children are parsed one by one: an interrupted
    /// write, whose last cluster still carries the size the writer reserved before it
    /// patched it at roll-over, and a source cut inside a cluster that declares a
    /// larger body. The first incomplete child ends recovery in both cases.
    fn read_cluster(&mut self, header: &Header, declared_end: Option<u64>) -> Result<(), Halt> {
        if header.size == Some(0) {
            self.notes.push(format!(
                "the cluster at byte {} declares an empty body, so it is read as the unterminated cluster of an interrupted write",
                header.start
            ));
        }
        let mut base: Option<u64> = None;
        loop {
            let position = self.source.position();
            if let Some(end) = declared_end
                && position >= end
            {
                return Ok(());
            }
            if position >= self.segment_end {
                if declared_end.is_some() {
                    return Err(Halt::corrupt(format!(
                        "the cluster at byte {} ends before the body it declares",
                        header.start
                    )));
                }
                return Ok(());
            }
            self.current_element_start = position;
            let Some(child) = self.source.read_header()? else {
                return Ok(());
            };
            if child.body_start > self.segment_end {
                return Err(Halt::incomplete(format!(
                    "the cluster child header at byte {} extends past the declared segment end",
                    child.start
                )));
            }
            if let Some(size) = child.size {
                let end = child.body_start.checked_add(size).ok_or_else(|| {
                    Halt::corrupt("a cluster child's size overflows the file offset")
                })?;
                if end > self.segment_end {
                    return Err(Halt::incomplete(format!(
                        "the {} element at byte {} declares {size} bytes but only {} remain",
                        element_name(child.id),
                        child.start,
                        self.segment_end - child.body_start
                    )));
                }
                if let Some(cluster_end) = declared_end
                    && end > cluster_end
                {
                    return Err(Halt::corrupt(format!(
                        "the {} element at byte {} reaches past the end of its cluster",
                        element_name(child.id),
                        child.start
                    )));
                }
            }
            match child.id {
                ID_TIMESTAMP => {
                    if base.is_some() {
                        return Err(Halt::corrupt(format!(
                            "the cluster at byte {} declares its timestamp twice",
                            header.start
                        )));
                    }
                    let size = self.bounded_size(&child, 8)?;
                    let mut field = [0u8; 8];
                    self.source.read_exact_into(&mut field[..size as usize])?;
                    base = Some(uint(&field[..size as usize]).map_err(Halt::corrupt)?);
                }
                ID_SIMPLE_BLOCK | ID_BLOCK_GROUP => {
                    let Some(base) = base else {
                        return Err(Halt::corrupt(format!(
                            "the cluster at byte {} holds a block before its timestamp",
                            header.start
                        )));
                    };
                    self.read_block(&child, base)?;
                }
                ID_VOID | ID_CRC32 => self.skip_element(&child)?,
                other if declared_end.is_none() && !is_cluster_child(other) => {
                    // The unterminated cluster ends here; the segment walk resumes at
                    // this element, which was read as a header only.
                    self.source.seek_to(child.start)?;
                    return Ok(());
                }
                other => {
                    return Err(Halt::corrupt(format!(
                        "the cluster at byte {} holds an unexpected {} element (0x{other:X}) at byte {}",
                        header.start,
                        element_name(other),
                        child.start
                    )));
                }
            }
        }
    }

    fn read_block(&mut self, header: &Header, base: u64) -> Result<(), Halt> {
        let limit = if header.id == ID_SIMPLE_BLOCK {
            MAX_BLOCK_BYTES
        } else {
            MAX_BLOCK_GROUP_BYTES
        };
        let size = self.bounded_size(header, limit)?;
        self.check_budget(size)?;
        let mut body = std::mem::take(&mut self.payload);
        let outcome = match self.source.read_body_into(size, &mut body) {
            Err(halt) => Err(halt),
            Ok(()) => {
                let parsed = if header.id == ID_SIMPLE_BLOCK {
                    parse_simple_block(&body)
                } else {
                    parse_block_group(&body)
                };
                match parsed {
                    Err(defect) => Err(Halt::corrupt(format!(
                        "the {} at byte {} is not a usable block: {defect}",
                        element_name(header.id),
                        header.start
                    ))),
                    Ok(parsed) => self.accept_block(base, &parsed),
                }
            }
        };
        self.payload = body;
        outcome
    }

    /// Validates one complete block and writes it into the new container.
    fn accept_block(&mut self, base: u64, block: &ParsedBlock<'_>) -> Result<(), Halt> {
        let Some(units) = i64::try_from(base)
            .ok()
            .and_then(|base| base.checked_add(i64::from(block.relative)))
            .and_then(|units| u64::try_from(units).ok())
        else {
            return Err(Halt::corrupt(format!(
                "a block's timestamp at cluster base {base} leaves the timeline"
            )));
        };
        let Some(metadata) = self.metadata.as_ref() else {
            return Err(Halt::corrupt("a block appears before any usable tracks"));
        };
        let is_video = block.track == metadata.video_track;
        let scale = metadata.info.timestamp_scale_ns;
        let Some(index) = metadata
            .tracks
            .iter()
            .position(|track| track.number == block.track)
        else {
            return Err(Halt::corrupt(format!(
                "the block at byte {} names track {}, which the Tracks element does not declare",
                self.current_element_start, block.track
            )));
        };
        let track = &metadata.tracks[index];
        if let Some(previous) = track.last_pts_units
            && units <= previous
        {
            return Err(Halt::corrupt(format!(
                "the block at byte {} maps to container unit {units}, which is not later than the previous block's {previous} on the same track",
                self.current_element_start
            )));
        }
        if block.keyframe && self.keyframes + 1 > MAX_KEYFRAMES {
            return Err(Halt::Fatal(backend_limit_error(format!(
                "Refusing keyframe {} because this writer indexes at most {MAX_KEYFRAMES} of them.",
                self.keyframes + 1
            ))));
        }
        if is_video {
            let validation = if track.codec_id == HEVC_CODEC_ID {
                crate::hevc::validate_recovered_access_unit(block.payload).and_then(|independent| {
                    if block.keyframe != independent {
                        Err("HEVC keyframe metadata disagrees with an IDR/BLA first slice (CRA recovery points are unsupported)".to_string())
                    } else {
                        Ok(())
                    }
                })
            } else if track.codec_id == AV1_CODEC_ID {
                crate::av1::validate_packet(block.payload, track.av1_sequence.as_deref()).and_then(|independent| {
                    if independent == block.keyframe { Ok(()) } else { Err("AV1 keyframe metadata disagrees with a sequence-bearing shown key Frame OBU".into()) }
                })
            } else {
                validate_avcc_payload(block.payload)
            };
            validation.map_err(|defect| {
                Halt::corrupt(format!(
                    "the video block at byte {} has unusable {} data: {defect}",
                    self.current_element_start, track.codec_id
                ))
            })?;
            if self.video_packets == 0 {
                self.first_video_is_keyframe = Some(block.keyframe);
            }
        }

        let output = self
            .output
            .as_mut()
            .expect("the output exists once a cluster has been opened");
        output.write_block(units, block).map_err(Halt::Fatal)?;

        let duration_units = block.duration_units.unwrap_or(0);
        let end_units = units.saturating_add(duration_units);
        self.end_units = self.end_units.max(end_units);
        let kind = metadata.tracks[index].kind;
        let track = &mut self.metadata.as_mut().expect("validated metadata").tracks[index];
        track.last_pts_units = Some(units);
        track.packets += 1;
        track.last_end_ns = track.last_end_ns.max(end_units.saturating_mul(scale));
        track.last_discard_padding_ns = block
            .discard_padding_ns
            .and_then(|padding| u64::try_from(padding).ok())
            .unwrap_or(0);
        if block.keyframe {
            track.keyframes += 1;
        }
        self.packets += 1;
        if is_video {
            self.video_packets += 1;
        } else if kind == RecoveredTrackKind::Audio {
            self.audio_packets += 1;
        }
        if block.keyframe && is_video {
            self.keyframes += 1;
        }
        Ok(())
    }
}

/// The trimmed end of the recovered timeline, computed exactly as the writer's own
/// finalization does once every audio track carried its trim.
fn trimmed_end_units(metadata: &Metadata) -> u64 {
    let mut end = 0u64;
    for track in &metadata.tracks {
        let units = if track.kind == RecoveredTrackKind::Audio {
            // The writer's finished audio end removes both the Opus lookahead
            // (CodecDelay) and the final packet's DiscardPadding. A video track
            // can end before audio during a static-screen interval, exposing a
            // missing delay in the recovered segment duration.
            match track
                .last_end_ns
                .checked_sub(track.codec_delay_ns.unwrap_or(0))
                .and_then(|end| end.checked_sub(track.last_discard_padding_ns))
            {
                Some(trimmed_ns) => to_units(trimmed_ns),
                None => to_units(track.last_end_ns),
            }
        } else {
            to_units(track.last_end_ns)
        };
        end = end.max(units);
    }
    end
}

/// The one video track the profile must declare.
fn single_video_track(tracks: &[TrackState]) -> Result<u64, String> {
    let video: Vec<&TrackState> = tracks
        .iter()
        .filter(|track| track.kind == RecoveredTrackKind::Video)
        .collect();
    match video.as_slice() {
        [] => Err("no video track is declared".to_string()),
        [track] => Ok(track.number),
        _ => Err(format!(
            "{} video tracks are declared where this profile has one",
            video.len()
        )),
    }
}

/// Parses and validates the whole `Tracks` element body.
fn parse_tracks(body: &[u8]) -> Result<Vec<TrackState>, String> {
    let mut slice = Slice::new(body);
    let mut tracks = Vec::new();
    while let Some((id, range)) = slice.read_header()? {
        if id != ID_TRACK_ENTRY {
            return Err(format!(
                "the Tracks element holds a {} element where a TrackEntry belongs",
                element_name(id)
            ));
        }
        if tracks.len() > super::MAX_AUDIO_TRACKS {
            return Err("the Tracks element exceeds its bounded header budget".to_string());
        }
        tracks.push(parse_track_entry(&body[range])?);
    }
    if tracks.is_empty() {
        return Err("the Tracks element declares no track".to_string());
    }
    for (index, track) in tracks.iter().enumerate() {
        if tracks[..index]
            .iter()
            .any(|other| other.number == track.number)
        {
            return Err(format!("track number {} is declared twice", track.number));
        }
    }
    let video = tracks
        .iter()
        .filter(|track| track.kind == RecoveredTrackKind::Video)
        .count();
    if video != 1 {
        return Err(format!(
            "{video} video tracks are declared where this profile has one"
        ));
    }
    Ok(tracks)
}

/// Parses and validates one `TrackEntry`, preserving every field it holds.
fn parse_track_entry(entry: &[u8]) -> Result<TrackState, String> {
    let mut number = None;
    let mut track_type = None;
    let mut codec_id = None;
    let mut codec_private = None;
    let mut name = None;
    let mut codec_delay_ns = None;
    let mut seek_pre_roll_ns = None;
    let mut has_video_settings = false;
    let mut slice = Slice::new(entry);
    while let Some((id, range)) = slice.read_header()? {
        let body = &entry[range];
        match id {
            ID_TRACK_NUMBER => {
                let value = uint(body)?;
                if value == 0 || value > MAX_TRACK_NUMBER {
                    return Err(format!(
                        "track number {value} is outside 1..={MAX_TRACK_NUMBER}"
                    ));
                }
                number = Some(value);
            }
            ID_TRACK_TYPE => track_type = Some(uint(body)?),
            ID_CODEC_ID => codec_id = Some(utf8(body)?),
            ID_CODEC_PRIVATE => codec_private = Some(body),
            ID_NAME => name = Some(utf8(body)?),
            ID_CODEC_DELAY => codec_delay_ns = Some(uint(body)?),
            ID_SEEK_PRE_ROLL => seek_pre_roll_ns = Some(uint(body)?),
            ID_CONTENT_ENCODINGS => {
                return Err(
                    "the track declares ContentEncodings, which this profile never writes"
                        .to_string(),
                );
            }
            ID_VIDEO => {
                has_video_settings = true;
                validate_video_settings(body)?;
            }
            ID_AUDIO => validate_audio_settings(body)?,
            _ => {}
        }
    }
    let number = number.ok_or_else(|| "a track declares no track number".to_string())?;
    let track_type = track_type.ok_or_else(|| format!("track {number} declares no track type"))?;
    let codec_id = codec_id.ok_or_else(|| format!("track {number} declares no codec"))?;
    let private =
        codec_private.ok_or_else(|| format!("track {number} declares no codec configuration"))?;
    let (kind, opus_pre_skip_samples) = match track_type {
        TRACK_TYPE_VIDEO => {
            if !has_video_settings {
                return Err(format!("video track {number} declares no video settings"));
            }
            match codec_id.as_str() {
                AVC_CODEC_ID => validate_avc_config(private),
                HEVC_CODEC_ID => crate::hevc::validate_decoder_configuration_record(private),
                AV1_CODEC_ID => crate::av1::validate_configuration(private),
                _ => Err(format!("unsupported video codec {codec_id}")),
            }
            .map_err(|defect| {
                format!("video track {number} has an unusable {codec_id} configuration: {defect}")
            })?;
            (RecoveredTrackKind::Video, None)
        }
        TRACK_TYPE_AUDIO => {
            if codec_id != OPUS_CODEC_ID {
                return Err(format!(
                    "audio track {number} declares the {codec_id} codec instead of {OPUS_CODEC_ID}"
                ));
            }
            let pre_skip = validate_opus_head(private).map_err(|defect| {
                format!("audio track {number} has an unusable OpusHead: {defect}")
            })?;
            if let Some(delay) = codec_delay_ns {
                let expected = samples_to_ns(u64::from(pre_skip));
                if delay != expected {
                    return Err(format!(
                        "audio track {number} declares a CodecDelay of {delay} ns where its OpusHead pre-skip of {pre_skip} samples is {expected} ns"
                    ));
                }
            }
            (RecoveredTrackKind::Audio, Some(pre_skip))
        }
        other => {
            return Err(format!(
                "track {number} declares the unsupported track type {other}"
            ));
        }
    };
    let av1_sequence = if codec_id == AV1_CODEC_ID {
        Some(crate::av1::sequence_payload(&private[4..])?.to_vec())
    } else {
        None
    };
    Ok(TrackState {
        number,
        kind,
        codec_id,
        name,
        codec_private_bytes: private.len(),
        av1_sequence,
        opus_pre_skip_samples,
        codec_delay_ns,
        seek_pre_roll_ns,
        last_pts_units: None,
        packets: 0,
        keyframes: 0,
        last_end_ns: 0,
        last_discard_padding_ns: 0,
    })
}

fn validate_video_settings(body: &[u8]) -> Result<(), String> {
    let mut slice = Slice::new(body);
    let mut width = None;
    let mut height = None;
    while let Some((id, range)) = slice.read_header()? {
        match id {
            ID_PIXEL_WIDTH => width = Some(uint(&body[range])?),
            ID_PIXEL_HEIGHT => height = Some(uint(&body[range])?),
            _ => {}
        }
    }
    for (value, name) in [(width, "PixelWidth"), (height, "PixelHeight")] {
        match value {
            Some(0) | None => return Err(format!("the video track declares no usable {name}")),
            Some(_) => {}
        }
    }
    Ok(())
}

fn validate_audio_settings(body: &[u8]) -> Result<(), String> {
    let mut slice = Slice::new(body);
    while let Some((id, range)) = slice.read_header()? {
        match id {
            ID_CHANNELS => {
                let channels = uint(&body[range])?;
                if channels == 0 || channels > 255 {
                    return Err(format!("the audio track declares {channels} channels"));
                }
            }
            ID_SAMPLING_FREQUENCY => {
                let rate = float(&body[range])?;
                if !(rate.is_finite() && rate > 0.0) {
                    return Err(format!("the audio track declares a {rate} Hz sample rate"));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Parses one `SimpleBlock` body.
fn parse_simple_block(data: &[u8]) -> Result<ParsedBlock<'_>, String> {
    let (track, relative, flags, payload) = split_block(data)?;
    Ok(ParsedBlock {
        track,
        relative,
        flags,
        simple: true,
        keyframe: flags & 0x80 != 0,
        duration_units: None,
        discard_padding_ns: None,
        extras: GroupExtras::default(),
        payload,
    })
}

/// Parses one `BlockGroup` body, keeping its own metadata elements byte for byte.
fn parse_block_group(data: &[u8]) -> Result<ParsedBlock<'_>, String> {
    let mut slice = Slice::new(data);
    let mut block = None;
    let mut extras = GroupExtras::default();
    let mut duration_units = None;
    let mut discard_padding_ns = None;
    while let Some((id, range)) = slice.read_header()? {
        let body = &data[range];
        match id {
            ID_BLOCK => {
                if block.is_some() {
                    return Err("the block group holds more than one block".to_string());
                }
                block = Some(body);
            }
            ID_BLOCK_DURATION => {
                extras.block_duration = RawField::new(body)
                    .map_err(|defect| format!("the block duration has {defect}"))?;
                duration_units = Some(uint(body)?);
            }
            ID_REFERENCE_BLOCK => {
                extras.reference_block = RawField::new(body)
                    .map_err(|defect| format!("the reference block has {defect}"))?;
                let _ = int(body)?;
            }
            ID_DISCARD_PADDING => {
                extras.discard_padding = RawField::new(body)
                    .map_err(|defect| format!("the discard padding has {defect}"))?;
                let padding = int(body)?;
                if padding < 0 {
                    return Err(format!("the discard padding is {padding} ns"));
                }
                discard_padding_ns = Some(padding);
            }
            ID_VOID | ID_CRC32 => {}
            other => {
                return Err(format!(
                    "the block group holds an unexpected {} element",
                    element_name(other)
                ));
            }
        }
    }
    let block = block.ok_or_else(|| "the block group holds no block".to_string())?;
    let (track, relative, flags, payload) = split_block(block)?;
    Ok(ParsedBlock {
        track,
        relative,
        flags,
        simple: false,
        // A block inside a block group carries no keyframe flag: the profile marks a
        // random access point by leaving the reference block out, exactly as the
        // writer does when it records its cue index.
        keyframe: !extras.reference_block.is_present(),
        duration_units,
        discard_padding_ns,
        extras,
        payload,
    })
}

/// Splits one block's track number, relative timestamp, flags and frame payload.
fn split_block(data: &[u8]) -> Result<(u64, i16, u8, &[u8]), String> {
    let mut slice = Slice::new(data);
    let track = slice.read_vint()?;
    if track == 0 || track > MAX_TRACK_NUMBER {
        return Err(format!("the block names track {track}"));
    }
    let header = slice.position;
    if data.len() < header + 3 {
        return Err("the block is shorter than its header".to_string());
    }
    let relative = i16::from_be_bytes([data[header], data[header + 1]]);
    let flags = data[header + 2];
    if flags & 0x06 != 0 {
        return Err("the block uses lacing, which this profile never writes".to_string());
    }
    let payload = &data[header + 3..];
    if payload.is_empty() {
        return Err("the block holds no frame data".to_string());
    }
    if payload.len() > MAX_PACKET_BYTES {
        return Err(format!(
            "the block holds {} bytes, above the {MAX_PACKET_BYTES} byte packet limit",
            payload.len()
        ));
    }
    Ok((track, relative, flags, payload))
}

/// Validates the length-prefixed NAL framing of one recovered video block.
fn validate_avcc_payload(payload: &[u8]) -> Result<(), String> {
    let mut position = 0usize;
    let mut units = 0usize;
    while position < payload.len() {
        if payload.len() - position < 4 {
            return Err(format!(
                "{} trailing bytes cannot hold a NAL length prefix",
                payload.len() - position
            ));
        }
        let length = u32::from_be_bytes([
            payload[position],
            payload[position + 1],
            payload[position + 2],
            payload[position + 3],
        ]) as usize;
        if length == 0 {
            return Err(format!("the NAL unit at byte {position} has no payload"));
        }
        position += 4;
        if payload.len() - position < length {
            return Err(format!(
                "the NAL unit at byte {position} declares {length} bytes but only {} remain",
                payload.len() - position
            ));
        }
        position += length;
        units += 1;
    }
    if units == 0 {
        return Err("the block holds no NAL unit".to_string());
    }
    Ok(())
}

/// Validates the AVC configuration record a video track stores as `CodecPrivate`.
fn validate_avc_config(record: &[u8]) -> Result<(), String> {
    if record.len() < 7 {
        return Err(format!(
            "{} bytes is shorter than a configuration record",
            record.len()
        ));
    }
    if record[0] != 1 {
        return Err(format!("configuration version {}", record[0]));
    }
    let length_size = (record[4] & 0x03) + 1;
    if length_size != 4 {
        return Err(format!(
            "{length_size}-byte NAL length prefixes where this profile stores four"
        ));
    }
    let sps = usize::from(record[5] & 0x1F);
    if sps == 0 {
        return Err("no sequence parameter set".to_string());
    }
    let mut position = 6usize;
    for index in 0..sps {
        position = skip_parameter_set(record, position)
            .map_err(|defect| format!("sequence parameter set {index} {defect}"))?;
    }
    let pps = usize::from(
        *record
            .get(position)
            .ok_or_else(|| "no picture parameter set count".to_string())?,
    );
    position += 1;
    if pps == 0 {
        return Err("no picture parameter set".to_string());
    }
    for index in 0..pps {
        position = skip_parameter_set(record, position)
            .map_err(|defect| format!("picture parameter set {index} {defect}"))?;
    }
    Ok(())
}

fn skip_parameter_set(record: &[u8], position: usize) -> Result<usize, String> {
    if record.len() < position + 2 {
        return Err("has no length".to_string());
    }
    let length = usize::from(u16::from_be_bytes([record[position], record[position + 1]]));
    let start = position + 2;
    if length == 0 {
        return Err("is empty".to_string());
    }
    if record.len() < start + length {
        return Err("is cut short".to_string());
    }
    Ok(start + length)
}

/// Validates the `OpusHead` a track stores as `CodecPrivate` and returns its pre-skip.
fn validate_opus_head(head: &[u8]) -> Result<u16, String> {
    if head.len() < OPUS_HEAD_MIN_BYTES {
        return Err(format!("{} bytes is shorter than an OpusHead", head.len()));
    }
    if &head[..8] != b"OpusHead" {
        return Err("the magic does not read OpusHead".to_string());
    }
    if head[8] != 1 {
        return Err(format!("OpusHead version {}", head[8]));
    }
    if head[9] == 0 {
        return Err("the channel count is zero".to_string());
    }
    Ok(u16::from_le_bytes([head[10], head[11]]))
}

/// Exact sample count to nanoseconds at the fixed 48 kHz rate.
fn samples_to_ns(samples: u64) -> u64 {
    samples.saturating_mul(1_000_000_000) / OPUS_SAMPLE_RATE
}

fn uint(body: &[u8]) -> Result<u64, String> {
    if body.is_empty() || body.len() > 8 {
        return Err(format!("a {}-byte unsigned integer", body.len()));
    }
    Ok(body
        .iter()
        .fold(0u64, |value, byte| (value << 8) | u64::from(*byte)))
}

fn int(body: &[u8]) -> Result<i64, String> {
    if body.is_empty() || body.len() > 8 {
        return Err(format!("a {}-byte signed integer", body.len()));
    }
    let mut value = if body[0] & 0x80 != 0 { -1i64 } else { 0 };
    for byte in body {
        value = (value << 8) | i64::from(*byte);
    }
    Ok(value)
}

fn float(body: &[u8]) -> Result<f64, String> {
    match body.len() {
        4 => Ok(f64::from(f32::from_be_bytes(
            body.try_into().expect("four bytes"),
        ))),
        8 => Ok(f64::from_be_bytes(body.try_into().expect("eight bytes"))),
        other => Err(format!("a {other}-byte float")),
    }
}

fn utf8(body: &[u8]) -> Result<String, String> {
    std::str::from_utf8(body)
        .map(str::to_owned)
        .map_err(|_| "a string that is not valid UTF-8".to_string())
}

/// Converts a `Duration` float into nanoseconds at `scale`.
fn scale_duration(body: &[u8], scale: u64) -> Result<u64, String> {
    let value = float(body)?;
    if !value.is_finite() || value < 0.0 {
        return Err(format!("the duration {value}"));
    }
    Ok((value * scale as f64) as u64)
}

/// The Matroska name of an element, for reports.
fn element_name(id: u32) -> &'static str {
    ids::lookup(ElementId(id)).map_or("unknown", |info| info.name)
}

/// True for the elements the profile permits inside a cluster.
fn is_cluster_child(id: u32) -> bool {
    matches!(
        id,
        ID_TIMESTAMP
            | ID_SIMPLE_BLOCK
            | ID_BLOCK_GROUP
            | ID_POSITION
            | ID_PREV_SIZE
            | ID_ENCRYPTED_BLOCK
            | ID_VOID
            | ID_CRC32
    )
}

fn source_error(message: impl Into<String>) -> RecorderError {
    writer_error(
        ErrorCode::UnsupportedCodec,
        RecoveryStatus::NoRecording,
        message,
        RECOVERY_ACTION,
    )
}

fn container_error(error: mkwriter::Error, what: &str) -> RecorderError {
    backend_error(error, what)
}

fn begin_master(
    out: &mut EbmlWriter<SinkAdapter>,
    id: ElementId,
) -> Result<MasterScope, RecorderError> {
    out.begin_streamed_master(id).map_err(|error| {
        container_error(error, "A recovered container element could not be opened.")
    })
}

fn end_master(out: &mut EbmlWriter<SinkAdapter>, scope: MasterScope) -> Result<(), RecorderError> {
    out.end_streamed_master(scope).map_err(|error| {
        container_error(error, "A recovered container element could not be closed.")
    })
}

/// Removes a file this run created exclusively.
fn discard_file(path: &Path) {
    let _ = std::fs::remove_file(path);
}

fn patch_at(
    out: &mut EbmlWriter<SinkAdapter>,
    offset: u64,
    buf: &[u8],
) -> Result<(), RecorderError> {
    out.get_mut()
        .patch(offset, buf)
        .map_err(|error| storage_error(&error, "The recovered container could not be patched."))
}

/// Writes one element with an exactly known body.
fn write_element(
    out: &mut EbmlWriter<SinkAdapter>,
    id: u32,
    body: &[u8],
) -> Result<(), RecorderError> {
    let mut header = Vec::with_capacity(12);
    ebml::write_id(&mut header, id)
        .map_err(|error| container_error(error, "An element identifier could not be written."))?;
    ebml::write_vint(&mut header, body.len() as u64)
        .map_err(|error| container_error(error, "An element size could not be written."))?;
    let sink = out.get_mut();
    sink.write_all(&header)
        .and_then(|()| sink.write_all(body))
        .map_err(|error| storage_error(&error, "The recovered container could not be written."))
}

/// Writes one block group: the block plus its own metadata, byte for byte.
///
/// The group's size is reserved at eight octets and patched once its body is on the
/// device, exactly as the writer's own backend frames a group. The body is validated
/// in memory before any of it is written, so a parse decision can never leave a group
/// half written; an I/O failure is fatal and removes the output.
fn write_block_group(
    out: &mut EbmlWriter<SinkAdapter>,
    block: &[u8],
    extras: &GroupExtras,
) -> Result<(), RecorderError> {
    let mut body = Vec::with_capacity(block.len() + 64);
    push_field(&mut body, ID_BLOCK, block)?;
    for (id, field) in [
        (ID_BLOCK_DURATION, extras.block_duration),
        (ID_REFERENCE_BLOCK, extras.reference_block),
        (ID_DISCARD_PADDING, extras.discard_padding),
    ] {
        if field.is_present() {
            push_field(&mut body, id, field.as_slice())?;
        }
    }
    let scope = begin_master(out, ids::BLOCK_GROUP)?;
    out.get_mut()
        .write_all(&body)
        .map_err(|error| storage_error(&error, "A block group could not be written."))?;
    end_master(out, scope)
}

fn push_field(body: &mut Vec<u8>, id: u32, payload: &[u8]) -> Result<(), RecorderError> {
    ebml::write_id(body, id)
        .map_err(|error| container_error(error, "An element identifier could not be written."))?;
    ebml::write_vint(body, payload.len() as u64)
        .map_err(|error| container_error(error, "An element size could not be written."))?;
    body.extend_from_slice(payload);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One element, encoded with the specification's own identifier and size writer.
    fn el(id: u32, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        ebml::write_id(&mut out, id).expect("an identifier");
        ebml::write_vint(&mut out, body.len() as u64).expect("a size");
        out.extend_from_slice(body);
        out
    }

    fn uint_el(id: u32, value: u64) -> Vec<u8> {
        el(id, &ebml::minimal_uint_bytes(value))
    }

    /// An AVCDecoderConfigurationRecord for the parameter sets the writer's tests use.
    fn avc_config() -> Vec<u8> {
        let sps: [u8; 8] = [0x67, 0x64, 0x00, 0x3C, 0xAC, 0x2B, 0x40, 0x14];
        let pps: [u8; 4] = [0x68, 0xEE, 0x3C, 0xB0];
        let mut config = vec![0, 0, 0, 1];
        config.extend_from_slice(&sps);
        config.extend_from_slice(&[0, 0, 0, 1]);
        config.extend_from_slice(&pps);
        crate::annexb::avc_decoder_configuration_record(&config).expect("a record")
    }

    fn opus_head(pre_skip: u16) -> Vec<u8> {
        let mut head = Vec::new();
        head.extend_from_slice(b"OpusHead");
        head.push(1);
        head.push(2);
        head.extend_from_slice(&pre_skip.to_le_bytes());
        head.extend_from_slice(&48_000u32.to_le_bytes());
        head.extend_from_slice(&0i16.to_le_bytes());
        head.push(0);
        head
    }

    fn track_entry(
        number: u64,
        track_type: u64,
        codec: &str,
        private: &[u8],
        codec_delay_ns: u64,
    ) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&uint_el(ID_TRACK_NUMBER, number));
        body.extend_from_slice(&uint_el(ID_TRACK_TYPE, track_type));
        body.extend_from_slice(&el(ID_CODEC_ID, codec.as_bytes()));
        body.extend_from_slice(&el(ID_CODEC_PRIVATE, private));
        if track_type == TRACK_TYPE_VIDEO {
            let mut video = Vec::new();
            video.extend_from_slice(&uint_el(ID_PIXEL_WIDTH, 64));
            video.extend_from_slice(&uint_el(ID_PIXEL_HEIGHT, 48));
            body.extend_from_slice(&el(ID_VIDEO, &video));
        } else {
            let mut audio = Vec::new();
            audio.extend_from_slice(&uint_el(ID_CHANNELS, 2));
            audio.extend_from_slice(&el(ID_SAMPLING_FREQUENCY, &48_000.0f64.to_be_bytes()));
            body.extend_from_slice(&el(ID_AUDIO, &audio));
            body.extend_from_slice(&uint_el(ID_CODEC_DELAY, codec_delay_ns));
            body.extend_from_slice(&uint_el(ID_SEEK_PRE_ROLL, 80_000_000));
            body.extend_from_slice(&el(ID_NAME, b"main"));
        }
        el(ID_TRACK_ENTRY, &body)
    }

    fn tracks_body() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&track_entry(
            1,
            TRACK_TYPE_VIDEO,
            AVC_CODEC_ID,
            &avc_config(),
            0,
        ));
        body.extend_from_slice(&track_entry(
            2,
            TRACK_TYPE_AUDIO,
            OPUS_CODEC_ID,
            &opus_head(312),
            6_500_000,
        ));
        body
    }

    /// A one-frame block: track number, relative timestamp, flags and payload.
    fn block_bytes(track: u64, relative: i16, flags: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        ebml::write_vint(&mut out, track).expect("a track number");
        out.extend_from_slice(&relative.to_be_bytes());
        out.push(flags);
        out.extend_from_slice(payload);
        out
    }

    /// The body offset of the element that starts at `start`, past both width fields.
    fn body_of(data: &[u8], start: usize) -> usize {
        let id_length = data[start].leading_zeros() as usize + 1;
        let size_at = start + id_length;
        size_at + 1 + (data[size_at].leading_zeros() as usize)
    }

    #[test]
    fn tracks_are_validated_field_by_field_and_preserved() {
        let tracks = parse_tracks(&tracks_body()).expect("valid tracks");
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].kind, RecoveredTrackKind::Video);
        assert_eq!(tracks[0].codec_id, AVC_CODEC_ID);
        assert_eq!(tracks[0].codec_private_bytes, avc_config().len());
        assert_eq!(tracks[1].kind, RecoveredTrackKind::Audio);
        assert_eq!(tracks[1].name.as_deref(), Some("main"));
        assert_eq!(tracks[1].opus_pre_skip_samples, Some(312));
        assert_eq!(tracks[1].codec_delay_ns, Some(6_500_000));
        assert_eq!(tracks[1].seek_pre_roll_ns, Some(80_000_000));
        assert_eq!(single_video_track(&tracks), Ok(1));
    }

    #[test]
    fn corrupt_metadata_is_refused() {
        let valid = tracks_body();
        let mut wrong_version = valid.clone();
        let position = wrong_version
            .windows(2)
            .position(|window| window == [0x63, 0xA2])
            .expect("the video codec private element");
        let record = body_of(&wrong_version, position);
        assert_eq!(wrong_version[record], 1, "configuration version");
        wrong_version[record] = 2;
        assert!(
            parse_tracks(&wrong_version)
                .expect_err("an unknown configuration version is refused")
                .contains("configuration version 2")
        );

        let mut no_video = Vec::new();
        no_video.extend_from_slice(&track_entry(
            2,
            TRACK_TYPE_AUDIO,
            OPUS_CODEC_ID,
            &opus_head(312),
            6_500_000,
        ));
        assert!(
            parse_tracks(&no_video)
                .expect_err("a file without a video track is refused")
                .contains("0 video tracks")
        );

        let mut broken_head = valid.clone();
        let position = broken_head
            .windows(2)
            .rposition(|window| window == [0x63, 0xA2])
            .expect("the audio codec private element");
        let record = body_of(&broken_head, position);
        assert_eq!(&broken_head[record..record + 8], b"OpusHead");
        broken_head[record] = b'X';
        assert!(
            parse_tracks(&broken_head)
                .expect_err("a broken OpusHead is refused")
                .contains("OpusHead")
        );

        let mut white_noise = valid.clone();
        white_noise[2] = 0x7F;
        assert!(parse_tracks(&white_noise).is_err());

        let mut no_type = valid.clone();
        let position = no_type
            .windows(2)
            .position(|window| window == [0x83, 0x81])
            .expect("the first track type");
        no_type[position] = 0x00;
        no_type[position + 1] = 0x00;
        assert!(parse_tracks(&no_type).is_err());
    }

    #[test]
    fn a_codec_delay_that_contradicts_its_opus_head_is_refused() {
        let mut body = Vec::new();
        body.extend_from_slice(&track_entry(
            1,
            TRACK_TYPE_VIDEO,
            AVC_CODEC_ID,
            &avc_config(),
            0,
        ));
        // Pre-skip 312 is 6_500_000 ns; 999 ns contradicts the OpusHead.
        body.extend_from_slice(&track_entry(
            2,
            TRACK_TYPE_AUDIO,
            OPUS_CODEC_ID,
            &opus_head(312),
            999,
        ));
        assert!(
            parse_tracks(&body)
                .expect_err("a contradictory CodecDelay is refused")
                .contains("CodecDelay")
        );
    }

    #[test]
    fn blocks_are_split_without_touching_their_payload() {
        let payload = [
            0x00, 0x00, 0x00, 0x02, 0x65, 0x11, 0x00, 0x00, 0x00, 0x01, 0x41,
        ];
        let data = block_bytes(1, -3, 0x00, &payload);
        let (track, relative, flags, frame) = split_block(&data).expect("a valid block");
        assert_eq!(track, 1);
        assert_eq!(relative, -3);
        assert_eq!(flags, 0x00);
        assert_eq!(frame, payload);
        validate_avcc_payload(frame).expect("length-prefixed NAL units");

        let mut laced = data.clone();
        laced[3] = 0x02;
        assert!(split_block(&laced).is_err(), "lacing is not this profile");

        let mut empty = block_bytes(1, 0, 0x80, &[0x00]);
        empty.pop();
        assert!(split_block(&empty).is_err(), "a block needs frame data");
        assert!(validate_avcc_payload(&[0, 0, 0, 4, 0x41]).is_err());
        assert!(validate_avcc_payload(&[0, 0, 0, 0, 0x41]).is_err());
        assert!(validate_avcc_payload(&[0, 0]).is_err());
    }

    #[test]
    fn a_block_group_keeps_its_own_metadata() {
        let payload = [0x00, 0x00, 0x00, 0x01, 0x41];
        let block = block_bytes(2, 7, 0x00, &payload);
        let mut body = Vec::new();
        body.extend_from_slice(&el(ID_BLOCK, &block));
        body.extend_from_slice(&uint_el(ID_BLOCK_DURATION, 1024));
        body.extend_from_slice(&el(ID_REFERENCE_BLOCK, &[0x00]));
        body.extend_from_slice(&el(ID_DISCARD_PADDING, &16u64.to_be_bytes()));
        let parsed = parse_block_group(&body).expect("a block group");
        assert_eq!(parsed.track, 2);
        assert_eq!(parsed.relative, 7);
        assert!(!parsed.simple);
        assert!(
            !parsed.keyframe,
            "a reference block marks a dependent frame"
        );
        assert_eq!(parsed.duration_units, Some(1024));
        assert_eq!(parsed.discard_padding_ns, Some(16));
        assert!(parsed.extras.reference_block.is_present());
        assert_eq!(
            parsed.extras.discard_padding.as_slice(),
            [0, 0, 0, 0, 0, 0, 0, 16]
        );
    }

    #[test]
    fn a_group_without_a_reference_block_is_a_keyframe() {
        let block = block_bytes(1, 0, 0x00, &[0x00, 0x00, 0x00, 0x01, 0x65]);
        let mut body = Vec::new();
        body.extend_from_slice(&el(ID_BLOCK, &block));
        body.extend_from_slice(&uint_el(ID_BLOCK_DURATION, 10));
        let parsed = parse_block_group(&body).expect("a block group");
        assert!(parsed.keyframe);
        assert!(parsed.extras.block_duration.is_present());
        assert!(!parsed.extras.reference_block.is_present());
    }

    #[test]
    fn negative_discard_padding_and_unknown_children_are_refused() {
        let block = block_bytes(1, 0, 0x00, &[0x41, 0x42]);
        let mut negative = el(ID_BLOCK, &block);
        negative.extend_from_slice(&el(ID_DISCARD_PADDING, &[0xFF]));
        assert!(
            parse_block_group(&negative)
                .expect_err("a negative padding is refused")
                .contains("discard padding")
        );
        let mut unknown = el(ID_BLOCK, &block);
        unknown.extend_from_slice(&[0xA7, 0x81, 0x01]);
        assert!(
            parse_block_group(&unknown)
                .expect_err("an unexpected child is refused")
                .contains("unexpected")
        );
        let mut without_block = uint_el(ID_BLOCK_DURATION, 10);
        without_block.extend_from_slice(&el(ID_REFERENCE_BLOCK, &[0x00]));
        assert!(parse_block_group(&without_block).is_err());
        let mut two_blocks = el(ID_BLOCK, &block);
        two_blocks.extend_from_slice(&el(ID_BLOCK, &block));
        assert!(parse_block_group(&two_blocks).is_err());
    }

    #[test]
    fn integers_and_scales_are_bounds_checked() {
        assert_eq!(uint(&[0x01, 0x00]), Ok(256));
        assert!(uint(&[]).is_err());
        assert!(uint(&[0u8; 9]).is_err());
        assert_eq!(int(&[0xFF]), Ok(-1));
        assert_eq!(float(&1.5f64.to_be_bytes()), Ok(1.5));
        assert_eq!(float(&1.5f32.to_be_bytes()), Ok(1.5));
        assert!(float(&[0, 0, 0]).is_err());
        assert_eq!(scale_duration(&2.0f64.to_be_bytes(), 1000), Ok(2000));
        assert!(scale_duration(&f64::NAN.to_be_bytes(), 1000).is_err());
        assert!(scale_duration(&(-1.0f64).to_be_bytes(), 1000).is_err());
        assert_eq!(samples_to_ns(312), 6_500_000);
        assert_eq!(utf8(b"main"), Ok("main".to_string()));
        assert!(utf8(&[0xFF]).is_err());
    }

    #[test]
    fn the_slice_reader_reports_truncation_instead_of_panicking() {
        let mut short = Slice::new(&[0xA1, 0x85, 0x81]);
        assert!(short.read_header().is_err());
        let mut cut = Slice::new(&[0xA1]);
        assert!(cut.read_header().is_err());
        let mut no_size = Slice::new(&[0xA1, 0x00]);
        assert!(no_size.read_header().is_err());
        let mut unknown = Slice::new(&[0xA1, 0xFF]);
        assert!(unknown.read_header().is_err());

        let mut complete = Slice::new(&[0xA1, 0x81, 0x81]);
        let (id, range) = complete.read_header().unwrap().expect("an element");
        assert_eq!(id, 0xA1);
        assert_eq!(&complete.data[range], [0x81]);
        assert!(complete.read_header().unwrap().is_none());

        let mut vint = Slice::new(&[0x81, 0x40, 0x40]);
        assert_eq!(vint.read_vint(), Ok(1));
        assert_eq!(vint.read_vint(), Ok(64));
        assert!(vint.read_vint().is_err());
    }

    #[test]
    fn element_names_come_from_the_specification_table() {
        assert_eq!(element_name(ID_CLUSTER), "Cluster");
        assert_eq!(element_name(ID_BLOCK_GROUP), "BlockGroup");
        assert_eq!(element_name(0x00), "unknown");
        assert!(is_cluster_child(ID_SIMPLE_BLOCK));
        assert!(!is_cluster_child(ID_TRACKS));
    }
}
