//! An atomic, bounded index of independently decodable recording segments.
//!
//! Each segment keeps its own Matroska Tracks/CodecPrivate header and shared clock
//! origin. Checkpoints are published after media flush/sync; only a finalized,
//! synced file receives a length and CRC-64. CRC detects accidental corruption;
//! it is not authentication and `sync_all` is not a power-loss guarantee.
use crate::journal::RecordingJournal;
use crate::{AvAudioTrack, DurableCheckpoint, MkvAvWriter, MkvVideoWriter, RecoverySummary};
use frame_clock::MediaTimestamp;
use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus};
use frame_encode::EncodedStreamInfo;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

pub(crate) mod retirement;

const MAGIC: &[u8; 8] = b"FREYMS01";
pub const MAX_SESSION_SEGMENTS: usize = 256;
pub const MAX_SESSION_MANIFEST_BYTES: usize = 2 * 1024 * 1024;
const MAX_TRACK_HEADER_BYTES: usize = 128 * 1024;
const MAX_NAME_BYTES: usize = 240;
const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentState {
    Open,
    Interrupted,
    Closed,
    /// A separately recovered prefix; its original final trim is still unknown.
    Recovered,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSegment {
    pub index: u32,
    /// One protected relative filename in the manifest's folder, never a path.
    pub file_name: String,
    pub origin_ns: u64,
    pub encoder_id: String,
    pub interruption_reason: Option<String>,
    /// Exact Matroska Tracks EBML written to this segment, including codec private,
    /// dimensions, colour, audio track order/labels, pre-skip and decoder delays.
    pub tracks_ebml: Vec<u8>,
    pub state: SegmentState,
    pub checkpoint: Option<DurableCheckpoint>,
    pub file_bytes: u64,
    pub crc64: u64,
    pub container_end_ns: u64,
    pub cues: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionManifest {
    pub version: u32,
    pub generation: u64,
    /// True only when every segment completed its own durable finalization.
    pub completed: bool,
    pub segments: Vec<SessionSegment>,
    /// Protected sibling sidecar; its framed records carry completion/recovery state.
    pub metadata_file: Option<String>,
}

struct State {
    path: PathBuf,
    manifest: SessionManifest,
    journal: Option<RecordingJournal>,
    failure: Option<String>,
    rolling: bool,
}

/// Owns the session index and its restart journal. One live segment at a time;
/// dropping an unfinished writer preserves an explicit interrupted entry.
pub struct RecordingSession {
    path: PathBuf,
    state: Arc<Mutex<State>>,
}

impl std::fmt::Debug for RecordingSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingSession")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl RecordingSession {
    pub fn create(path: &Path) -> Result<Self, RecorderError> {
        let path = absolute_manifest_path(path).map_err(session_error)?;
        if path.exists() {
            return Err(session_error(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Session manifest already exists.",
            )));
        }
        let journal = RecordingJournal::create(&path).map_err(session_error)?;
        let manifest = SessionManifest {
            version: MANIFEST_VERSION,
            generation: 0,
            completed: false,
            segments: Vec::new(),
            metadata_file: None,
        };
        atomic_store(&path, &manifest, false).map_err(session_error)?;
        Ok(Self {
            path: path.clone(),
            state: Arc::new(Mutex::new(State {
                path,
                manifest,
                journal: Some(journal),
                failure: None,
                rolling: false,
            })),
        })
    }

    pub fn manifest_path(&self) -> &Path {
        &self.path
    }

    /// Deterministic association only; create_new still reserves the actual file.
    pub fn metadata_path(&self) -> PathBuf {
        metadata_path(&self.path)
    }

    /// Publish the reference after the writer durably created its header. This is
    /// control-thread work, never a capture callback or per-event manifest write.
    pub fn attach_metadata(&mut self) -> Result<(), RecorderError> {
        let path = self.metadata_path();
        if !fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file()) {
            return Err(session_error(invalid(
                "Metadata sidecar is not a regular file.",
            )));
        }
        let name = path
            .file_name()
            .unwrap()
            .to_str()
            .ok_or_else(|| session_error(invalid("Invalid metadata name.")))?
            .to_owned();
        let mut state = self
            .state
            .lock()
            .map_err(|_| session_error(invalid("Session lock failed.")))?;
        update(&mut state, |manifest| {
            if manifest.completed || manifest.metadata_file.is_some() {
                return Err(invalid("Metadata already attached or session complete."));
            }
            manifest.version = 2;
            manifest.metadata_file = Some(name);
            Ok(())
        })
        .map_err(session_error)
    }

    pub fn snapshot(&self) -> Result<SessionManifest, RecorderError> {
        let state = self
            .state
            .lock()
            .map_err(|_| session_error(io::Error::other("Session index lock failed.")))?;
        check_state(&state).map_err(session_error)?;
        Ok(state.manifest.clone())
    }

    pub fn next_segment_path(&self) -> Result<PathBuf, RecorderError> {
        let state = self
            .state
            .lock()
            .map_err(|_| session_error(io::Error::other("Session index lock failed.")))?;
        check_state(&state).map_err(session_error)?;
        if state.manifest.completed
            || state.manifest.segments.len() >= MAX_SESSION_SEGMENTS
            || state
                .manifest
                .segments
                .last()
                .is_some_and(|segment| segment.state == SegmentState::Open)
        {
            return Err(session_error(io::Error::other(
                "Session is complete, full, or already has a live segment.",
            )));
        }
        Ok(self
            .path
            .parent()
            .expect("validated parent")
            .join(if state.rolling {
                format!(
                    "freym-{:016x}-rolling-{:016x}.mkv",
                    checksum(
                        self.path
                            .file_name()
                            .expect("validated name")
                            .to_string_lossy()
                            .as_bytes()
                    ),
                    state.manifest.generation
                )
            } else {
                segment_name(&self.path, state.manifest.segments.len())
            }))
    }

    /// Only a dedicated replay store may opt into retirement. Ordinary recording
    /// sessions always preserve their interrupted and verified original segments.
    pub(crate) fn enable_rolling(&mut self) -> Result<(), RecorderError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| session_error(invalid("Session lock failed.")))?;
        check_state(&state).map_err(session_error)?;
        if !state.manifest.segments.is_empty() || state.manifest.completed {
            return Err(session_error(invalid(
                "Rolling must be selected before the first segment.",
            )));
        }
        state.rolling = true;
        Ok(())
    }

    /// Publish a durable retirement intent before compacting references. Files
    /// are deleted only after the exact new manifest and every retired CRC are
    /// verified; an interrupted deletion remains resumable offline.
    pub(crate) fn retire_replay_prefix(&mut self, count: usize) -> Result<(), RecorderError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| session_error(invalid("Session lock failed.")))?;
        check_state(&state).map_err(session_error)?;
        if !state.rolling
            || state.manifest.completed
            || count == 0
            || count > state.manifest.segments.len()
            || state
                .manifest
                .segments
                .iter()
                .any(|s| s.state != SegmentState::Closed)
        {
            return Err(session_error(invalid(
                "Only sealed replay segments can be retired.",
            )));
        }
        retirement::prepare(&state.path, &state.manifest, count).map_err(session_error)?;
        update(&mut state, |manifest| {
            manifest.segments.drain(..count);
            for (index, segment) in manifest.segments.iter_mut().enumerate() {
                segment.index = index as u32;
            }
            Ok(())
        })
        .map_err(session_error)?;
        retirement::cleanup(&state.path).map_err(session_error)?;
        Ok(())
    }

    pub fn create_video_segment(
        &mut self,
        stream: &EncodedStreamInfo,
        origin: MediaTimestamp,
    ) -> Result<MkvVideoWriter, RecorderError> {
        let (path, lease) = self.begin(origin, &stream.capability.id)?;
        MkvVideoWriter::create_segment(&path, stream, origin, lease)
    }

    pub fn create_av_segment(
        &mut self,
        stream: &EncodedStreamInfo,
        tracks: &[AvAudioTrack],
        origin: MediaTimestamp,
        live: bool,
    ) -> Result<MkvAvWriter, RecorderError> {
        let (path, lease) = self.begin(origin, &stream.capability.id)?;
        MkvAvWriter::create_segment(&path, stream, tracks, origin, live, lease)
    }

    fn begin(
        &mut self,
        origin: MediaTimestamp,
        encoder_id: &str,
    ) -> Result<(PathBuf, SegmentLease), RecorderError> {
        let path = self.next_segment_path()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| session_error(io::Error::other("Session index lock failed.")))?;
        let index = state.manifest.segments.len();
        update(&mut state, |manifest| {
            if manifest
                .segments
                .last()
                .is_some_and(|previous| origin.0 < previous.origin_ns)
            {
                return Err(invalid(
                    "A new segment cannot regress its shared clock origin.",
                ));
            }
            manifest.segments.push(SessionSegment {
                index: index as u32,
                file_name: path
                    .file_name()
                    .expect("generated name")
                    .to_str()
                    .expect("ASCII name")
                    .into(),
                origin_ns: origin.0,
                encoder_id: encoder_id.into(),
                interruption_reason: None,
                tracks_ebml: Vec::new(),
                state: SegmentState::Open,
                checkpoint: None,
                file_bytes: 0,
                crc64: 0,
                container_end_ns: 0,
                cues: 0,
            });
            Ok(())
        })
        .map_err(session_error)?;
        Ok((
            path,
            SegmentLease {
                state: Arc::clone(&self.state),
                index,
                finalized: None,
                completed: false,
            },
        ))
    }

    pub fn record_interruption_reason(&mut self, reason: &str) -> Result<(), RecorderError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| session_error(io::Error::other("Session index lock failed.")))?;
        update(&mut state, |manifest| {
            let segment = manifest
                .segments
                .last_mut()
                .ok_or_else(|| invalid("No live segment to annotate."))?;
            if !matches!(
                segment.state,
                SegmentState::Open | SegmentState::Interrupted
            ) || reason.is_empty()
                || reason.len() > 4096
            {
                return Err(invalid("Invalid segment interruption metadata."));
            }
            segment.interruption_reason = Some(reason.into());
            Ok(())
        })
        .map_err(session_error)
    }

    /// A session with interrupted segments remains explicitly incomplete, even if
    /// a later fallback segment finalized. It can still be recovered as segments.
    pub fn finish(&mut self) -> Result<SessionManifest, RecorderError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| session_error(io::Error::other("Session index lock failed.")))?;
        if state
            .manifest
            .segments
            .iter()
            .any(|segment| segment.state == SegmentState::Open)
        {
            return Err(session_error(io::Error::other(
                "Finish the live segment before finishing its session.",
            )));
        }
        update(&mut state, |manifest| {
            manifest.completed = !manifest.segments.is_empty()
                && manifest
                    .segments
                    .iter()
                    .all(|segment| segment.state == SegmentState::Closed);
            Ok(())
        })
        .map_err(session_error)?;
        if state.manifest.completed
            && let Some(journal) = state.journal.take()
        {
            journal.complete().map_err(session_error)?;
        }
        Ok(state.manifest.clone())
    }
}

pub(crate) struct SegmentLease {
    state: Arc<Mutex<State>>,
    index: usize,
    finalized: Option<(u64, u64)>,
    completed: bool,
}

impl SegmentLease {
    pub(crate) fn headers(&mut self, headers: &[u8]) -> io::Result<()> {
        if headers.is_empty() || headers.len() > MAX_TRACK_HEADER_BYTES {
            return Err(invalid("Segment track header exceeds its bound."));
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("Session index lock failed."))?;
        update(&mut state, |manifest| {
            manifest.segments[self.index].tracks_ebml = headers.to_vec();
            Ok(())
        })
    }

    pub(crate) fn checkpoint(&mut self, checkpoint: DurableCheckpoint) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("Session index lock failed."))?;
        update(&mut state, |manifest| {
            let segment = &mut manifest.segments[self.index];
            if segment.checkpoint.is_some_and(|old| {
                checkpoint.sequence <= old.sequence
                    || checkpoint.position_bytes < old.position_bytes
                    || checkpoint.container_end_ns < old.container_end_ns
            }) {
                return Err(invalid("Checkpoint regressed."));
            }
            segment.checkpoint = Some(checkpoint);
            Ok(())
        })
    }

    pub(crate) fn finalization(&mut self, end_ns: u64, cues: u64) {
        self.finalized = Some((end_ns, cues));
    }

    pub(crate) fn complete(&mut self, path: &Path) -> io::Result<()> {
        let (end_ns, cues) = self
            .finalized
            .ok_or_else(|| invalid("Segment has no finalized container index."))?;
        let (file_bytes, crc64) = file_integrity(path)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("Session index lock failed."))?;
        update(&mut state, |manifest| {
            let segment = &mut manifest.segments[self.index];
            if segment.state != SegmentState::Open || segment.interruption_reason.is_some() {
                return Err(invalid(
                    "An interrupted segment cannot complete successfully.",
                ));
            }
            if segment.tracks_ebml.is_empty() || cues == 0 || file_bytes == 0 {
                return Err(invalid("Finalized segment lacks tracks or an index."));
            }
            segment.state = SegmentState::Closed;
            segment.file_bytes = file_bytes;
            segment.crc64 = crc64;
            segment.container_end_ns = end_ns;
            segment.cues = cues;
            Ok(())
        })?;
        self.completed = true;
        Ok(())
    }
}

impl Drop for SegmentLease {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            // Failure remains sticky so a failed manifest publication cannot be
            // ignored by opening a fallback segment. The last durable index stays.
            let _ = update(&mut state, |manifest| {
                manifest.segments[self.index].state = SegmentState::Interrupted;
                Ok(())
            });
        }
    }
}

fn check_state(state: &State) -> io::Result<()> {
    match &state.failure {
        Some(error) => Err(io::Error::other(error.clone())),
        None => Ok(()),
    }
}

fn update(
    state: &mut State,
    change: impl FnOnce(&mut SessionManifest) -> io::Result<()>,
) -> io::Result<()> {
    check_state(state)?;
    let mut next = state.manifest.clone();
    change(&mut next)?;
    next.generation = next
        .generation
        .checked_add(1)
        .ok_or_else(|| invalid("Manifest generation exhausted."))?;
    if let Err(error) = atomic_store(&state.path, &next, true) {
        state.failure = Some(format!("Session manifest publication failed: {error}"));
        return Err(error);
    }
    state.manifest = next;
    Ok(())
}

/// Loads and validates the atomic session index without touching media files.
pub fn read_session_manifest(path: &Path) -> Result<SessionManifest, RecorderError> {
    read_manifest(path).map_err(session_error)
}

/// Read-only admission for consumers of finalized segments.
/// Reuses the recovery integrity calculation; never repairs or changes a source.
pub fn verify_session_segment(path: &Path, segment: &SessionSegment) -> Result<(), RecorderError> {
    if !matches!(
        segment.state,
        SegmentState::Closed | SegmentState::Recovered
    ) {
        return Err(session_error(invalid(
            "Segment is still open or interrupted; recover it before editing.",
        )));
    }
    if file_integrity(path).map_err(session_error)? != (segment.file_bytes, segment.crc64) {
        return Err(session_error(invalid(
            "Segment length/CRC does not match its durable manifest.",
        )));
    }
    Ok(())
}

fn read_manifest(path: &Path) -> io::Result<SessionManifest> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > MAX_SESSION_MANIFEST_BYTES as u64 {
        return Err(invalid("Manifest is not a bounded regular file."));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)?
        .take(MAX_SESSION_MANIFEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    decode(&bytes)
}

#[derive(Debug)]
pub struct SessionRecoverySummary {
    pub source: PathBuf,
    pub output: PathBuf,
    pub manifest: SessionManifest,
    pub verified_closed_segments: u32,
    pub recovered_tails: Vec<RecoverySummary>,
    /// Entries with corrupt finalized bytes or no recoverable random access point.
    /// Originals and verified closed segments are always retained.
    pub rejected_segments: Vec<(String, String)>,
    pub metadata_recovery: Option<frame_metadata::ImportSummary>,
    pub metadata_error: Option<String>,
}

/// Verifies closed segment bytes and copies them unchanged into exclusively new
/// files. Interrupted segments are recovered separately; no track/config merge.
/// The originals are never opened for writing or removed, including on failure.
pub fn recover_session(
    source: &Path,
    output: &Path,
) -> Result<SessionRecoverySummary, RecorderError> {
    copy_session(source, output, false, false)
}

/// The rolling store calls this under its exclusive control-thread ownership;
/// its journal stays locked and every entry must be finalized. Recovery callers
/// continue to require an abandoned-session lock and preserve interrupted tails.
pub(crate) fn export_closed_snapshot(
    source: &Path,
    output: &Path,
) -> Result<SessionRecoverySummary, RecorderError> {
    copy_session(source, output, true, true)
}

/// Archive and journal ownership is already held by offline replay recovery.
pub(crate) fn recover_owned_snapshot(
    source: &Path,
    output: &Path,
) -> Result<SessionRecoverySummary, RecorderError> {
    copy_session(source, output, false, true)
}

fn copy_session(
    source: &Path,
    output: &Path,
    sealed_live_snapshot: bool,
    ownership_held: bool,
) -> Result<SessionRecoverySummary, RecorderError> {
    let source = absolute_manifest_path(source).map_err(session_error)?;
    let output = absolute_manifest_path(output).map_err(session_error)?;
    let _guard = if ownership_held {
        None
    } else {
        abandoned_session_guard(&source).map_err(session_error)?
    };
    if output.exists() {
        return Err(session_error(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Recovery manifest already exists.",
        )));
    }
    let original = read_manifest(&source).map_err(session_error)?;
    if sealed_live_snapshot
        && original
            .segments
            .iter()
            .any(|s| s.state != SegmentState::Closed)
    {
        return Err(session_error(invalid(
            "Replay export requires only closed segments.",
        )));
    }
    let source_dir = source.parent().expect("validated parent");
    let output_dir = output.parent().expect("validated parent");
    let mut restored = SessionManifest {
        version: MANIFEST_VERSION,
        generation: 0,
        completed: false,
        segments: Vec::new(),
        metadata_file: None,
    };
    // Publish ownership before copying and checkpoint each verified segment.
    // Interrupted exports are discoverable and never masquerade as completed.
    let journal = RecordingJournal::create(&output).map_err(session_error)?;
    atomic_store(&output, &restored, false).map_err(session_error)?;
    let mut verified = 0;
    let mut tails = Vec::new();
    let mut rejected = Vec::new();
    for segment in &original.segments {
        let source_path = source_dir.join(&segment.file_name);
        let destination_name = generated_segment_name(&output, restored.segments.len());
        let destination = output_dir.join(&destination_name);
        if matches!(
            segment.state,
            SegmentState::Closed | SegmentState::Recovered
        ) {
            match file_integrity(&source_path) {
                Ok(measured) if measured == (segment.file_bytes, segment.crc64) => {
                    copy_new_verified(&source_path, &destination, measured).map_err(session_error)?;
                    let mut closed = segment.clone();
                    closed.index = restored.segments.len() as u32;
                    closed.file_name = destination_name;
                    restored.segments.push(closed);
                    publish_copy_checkpoint(&output, &mut restored).map_err(session_error)?;
                    verified += 1;
                }
                Ok(_) => rejected.push((segment.file_name.clone(), "Finalized length/CRC does not match its durable manifest; segment was preserved without declaring it valid.".into())),
                Err(error) => rejected.push((segment.file_name.clone(), error.to_string())),
            }
        } else {
            if segment.tracks_ebml.is_empty() {
                rejected.push((
                    segment.file_name.clone(),
                    "Segment never published track ownership metadata; source was preserved."
                        .into(),
                ));
                continue;
            }
            if !fs::symlink_metadata(&source_path).is_ok_and(|meta| meta.is_file()) {
                rejected.push((
                    segment.file_name.clone(),
                    "Interrupted segment is not a regular file.".into(),
                ));
                continue;
            }
            if segment.checkpoint.is_some_and(|point| {
                fs::metadata(&source_path).is_ok_and(|meta| point.position_bytes > meta.len())
            }) {
                rejected.push((
                    segment.file_name.clone(),
                    "Segment is shorter than its durable checkpoint; original was preserved."
                        .into(),
                ));
                continue;
            }
            match crate::mkv::recovery::recover_segment(
                &source_path,
                &destination,
                &segment.tracks_ebml,
            ) {
                Ok(summary) => {
                    let (file_bytes, crc64) =
                        file_integrity(&destination).map_err(session_error)?;
                    let mut recovered = segment.clone();
                    recovered.index = restored.segments.len() as u32;
                    recovered.file_name = destination_name;
                    recovered.state = SegmentState::Recovered;
                    recovered.checkpoint = None;
                    recovered.file_bytes = file_bytes;
                    recovered.crc64 = crc64;
                    recovered.container_end_ns = summary.container_end_ns;
                    recovered.cues = summary.cues;
                    restored.segments.push(recovered);
                    publish_copy_checkpoint(&output, &mut restored).map_err(session_error)?;
                    tails.push(summary);
                }
                Err(error) => rejected.push((segment.file_name.clone(), error.message)),
            }
        }
    }
    if restored.segments.is_empty() {
        return Err(session_error(invalid(
            "No verified segment or playable prefix remains; originals were preserved.",
        )));
    }
    if sealed_live_snapshot {
        if !rejected.is_empty() || restored.segments.len() != original.segments.len() {
            return Err(session_error(invalid(
                "Replay export rejected a segment; partial files were preserved.",
            )));
        }
        restored.completed = true;
    }
    let mut metadata_recovery = None;
    let mut metadata_error = None;
    if let Some(name) = &original.metadata_file {
        let destination = metadata_path(&output);
        match frame_metadata::recover_sidecar(&source_dir.join(name), &destination) {
            Ok(summary) => {
                restored.version = 2;
                restored.metadata_file = Some(
                    destination
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
                metadata_recovery = Some(summary);
            }
            Err(error) => metadata_error = Some(error.to_string()),
        }
    }
    // Recovered sessions retain a journal: repaired prefixes are never presented
    // as successfully completed original recordings.
    publish_copy_checkpoint(&output, &mut restored).map_err(session_error)?;
    if restored.completed {
        journal.complete().map_err(session_error)?;
    } else {
        drop(journal);
    }
    Ok(SessionRecoverySummary {
        source,
        output,
        manifest: restored,
        verified_closed_segments: verified,
        recovered_tails: tails,
        rejected_segments: rejected,
        metadata_recovery,
        metadata_error,
    })
}

fn publish_copy_checkpoint(output: &Path, manifest: &mut SessionManifest) -> io::Result<()> {
    manifest.generation = manifest
        .generation
        .checked_add(1)
        .ok_or_else(|| invalid("Export generation exhausted."))?;
    atomic_store(output, manifest, true)
}

fn abandoned_session_guard(source: &Path) -> io::Result<Option<File>> {
    let mut marker = source.as_os_str().to_os_string();
    marker.push(".freym-incomplete");
    let marker = PathBuf::from(marker);
    match fs::symlink_metadata(&marker) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err(invalid("Session marker is not a regular file.")),
        Err(error) => return Err(error),
    }
    let file = OpenOptions::new().read(true).write(true).open(marker)?;
    file.try_lock().map_err(|error| {
        io::Error::other(format!(
            "Session is still in use or cannot be locked: {error}"
        ))
    })?;
    Ok(Some(file))
}

fn copy_new_verified(source: &Path, output: &Path, expected: (u64, u64)) -> io::Result<()> {
    let mut input = File::open(source)?;
    let mut target = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    io::copy(&mut input, &mut target)?;
    target.sync_all()?;
    drop(target);
    if file_integrity(output)? != expected {
        return Err(invalid(
            "Segment changed while recovery copied it; source was preserved.",
        ));
    }
    Ok(())
}

fn absolute_manifest_path(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(invalid("Choose an absolute session manifest path."));
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("Manifest filename is not valid UTF-8."))?;
    validate_name(name)?;
    let parent = path
        .parent()
        .ok_or_else(|| invalid("Manifest has no parent folder."))?
        .canonicalize()?;
    Ok(parent.join(name))
}

fn validate_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.len() > MAX_NAME_BYTES
        || name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|ch| ch.is_control() || "<>:\"/\\|?*".contains(ch))
        || Path::new(name).components().count() != 1
        || !matches!(
            Path::new(name).components().next(),
            Some(Component::Normal(_))
        )
    {
        return Err(invalid(
            "Session index filename is not a protected relative name.",
        ));
    }
    let base = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    if [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ]
    .contains(&base.as_str())
    {
        return Err(invalid("Reserved session index filename."));
    }
    Ok(())
}

fn metadata_path(manifest: &Path) -> PathBuf {
    let hash = checksum(
        manifest
            .file_name()
            .expect("validated name")
            .to_string_lossy()
            .as_bytes(),
    );
    manifest.with_file_name(format!("freym-{hash:016x}.freym-events"))
}

fn segment_name(manifest: &Path, index: usize) -> String {
    let seed = manifest
        .file_name()
        .expect("validated name")
        .to_string_lossy();
    if index == 0
        && let Some(original) = seed.strip_suffix(".freym-session")
        && validate_name(original).is_ok()
    {
        return original.into();
    }
    generated_segment_name(manifest, index)
}

fn generated_segment_name(manifest: &Path, index: usize) -> String {
    let seed = manifest
        .file_name()
        .expect("validated name")
        .to_string_lossy();
    format!(
        "freym-{:016x}-segment-{index:04}.mkv",
        checksum(seed.as_bytes())
    )
}

fn validate(manifest: &SessionManifest) -> io::Result<()> {
    if !matches!(manifest.version, 1 | 2) || manifest.segments.len() > MAX_SESSION_SEGMENTS {
        return Err(invalid(
            "Unknown manifest version or segment bound exceeded.",
        ));
    }
    if let Some(name) = &manifest.metadata_file {
        validate_name(name)?;
        if manifest.version < 2
            || !name.ends_with(".freym-events")
            || manifest
                .segments
                .iter()
                .any(|s| s.file_name.eq_ignore_ascii_case(name))
        {
            return Err(invalid("Invalid metadata sidecar reference."));
        }
    }
    for (index, segment) in manifest.segments.iter().enumerate() {
        validate_name(&segment.file_name)?;
        if segment.encoder_id.is_empty()
            || segment.encoder_id.len() > 1024
            || segment
                .interruption_reason
                .as_ref()
                .is_some_and(|reason| reason.is_empty() || reason.len() > 4096)
        {
            return Err(invalid("Unbounded or empty encoder/interruption metadata."));
        }
        if segment.index as usize != index
            || manifest.segments[..index]
                .iter()
                .any(|previous| previous.file_name.eq_ignore_ascii_case(&segment.file_name))
            || segment.tracks_ebml.len() > MAX_TRACK_HEADER_BYTES
        {
            return Err(invalid("Invalid, duplicate or unbounded segment index."));
        }
        if index > 0 && segment.origin_ns < manifest.segments[index - 1].origin_ns {
            return Err(invalid("Segment shared clock origins regressed."));
        }
        if segment.state == SegmentState::Open && index + 1 != manifest.segments.len() {
            return Err(invalid("Only the final segment can be live."));
        }
        if segment.state == SegmentState::Closed && segment.interruption_reason.is_some() {
            return Err(invalid("An interrupted segment cannot be declared closed."));
        }
        if matches!(
            segment.state,
            SegmentState::Open | SegmentState::Interrupted
        ) && (segment.file_bytes != 0
            || segment.crc64 != 0
            || segment.container_end_ns != 0
            || segment.cues != 0)
        {
            return Err(invalid(
                "An unfinished segment cannot contain final integrity metadata.",
            ));
        }
        if let Some(point) = segment.checkpoint
            && (point.sequence == 0 || point.position_bytes == 0)
        {
            return Err(invalid("Invalid checkpoint index."));
        }
        if matches!(
            segment.state,
            SegmentState::Closed | SegmentState::Recovered
        ) && (segment.tracks_ebml.is_empty()
            || segment.file_bytes == 0
            || segment.cues == 0
            || segment
                .checkpoint
                .is_some_and(|point| point.position_bytes > segment.file_bytes))
        {
            return Err(invalid(
                "Finalized segment has incomplete integrity metadata.",
            ));
        }
    }
    if manifest.completed
        && (manifest.segments.is_empty()
            || manifest
                .segments
                .iter()
                .any(|segment| segment.state != SegmentState::Closed))
    {
        return Err(invalid(
            "Incomplete segments cannot form a completed session.",
        ));
    }
    Ok(())
}

fn encode(manifest: &SessionManifest) -> io::Result<Vec<u8>> {
    validate(manifest)?;
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    put_u32(&mut out, manifest.version);
    put_u64(&mut out, manifest.generation);
    out.push(u8::from(manifest.completed));
    put_u32(&mut out, manifest.segments.len() as u32);
    for segment in &manifest.segments {
        put_u32(&mut out, segment.index);
        put_blob(&mut out, segment.file_name.as_bytes());
        put_u64(&mut out, segment.origin_ns);
        put_blob(&mut out, segment.encoder_id.as_bytes());
        out.push(u8::from(segment.interruption_reason.is_some()));
        if let Some(reason) = &segment.interruption_reason {
            put_blob(&mut out, reason.as_bytes());
        }
        put_blob(&mut out, &segment.tracks_ebml);
        out.push(match segment.state {
            SegmentState::Open => 0,
            SegmentState::Interrupted => 1,
            SegmentState::Closed => 2,
            SegmentState::Recovered => 3,
        });
        out.push(u8::from(segment.checkpoint.is_some()));
        if let Some(point) = segment.checkpoint {
            put_u64(&mut out, point.sequence);
            put_u64(&mut out, point.position_bytes);
            put_u64(&mut out, point.container_end_ns);
        }
        for value in [
            segment.file_bytes,
            segment.crc64,
            segment.container_end_ns,
            segment.cues,
        ] {
            put_u64(&mut out, value);
        }
        if out.len() + 8 > MAX_SESSION_MANIFEST_BYTES {
            return Err(invalid("Session manifest exceeded its byte bound."));
        }
    }
    if manifest.version >= 2 {
        out.push(u8::from(manifest.metadata_file.is_some()));
        if let Some(name) = &manifest.metadata_file {
            put_blob(&mut out, name.as_bytes());
        }
    }
    if out.len() + 8 > MAX_SESSION_MANIFEST_BYTES {
        return Err(invalid("Session manifest exceeded its byte bound."));
    }
    let crc = checksum(&out);
    put_u64(&mut out, crc);
    Ok(out)
}

fn decode(bytes: &[u8]) -> io::Result<SessionManifest> {
    if bytes.len() < 33 || bytes.len() > MAX_SESSION_MANIFEST_BYTES {
        return Err(invalid("Manifest byte bound or header is invalid."));
    }
    let (body, digest) = bytes.split_at(bytes.len() - 8);
    if checksum(body) != u64::from_le_bytes(digest.try_into().expect("eight bytes")) {
        return Err(invalid("Manifest integrity check failed."));
    }
    let mut reader = Reader(body);
    if reader.bytes(8)? != MAGIC {
        return Err(invalid("Unknown session manifest header."));
    }
    let version = reader.u32()?;
    let generation = reader.u64()?;
    let completed = reader.flag()?;
    let count = reader.u32()? as usize;
    if !matches!(version, 1 | 2) || count > MAX_SESSION_SEGMENTS {
        return Err(invalid(
            "Unknown manifest version or segment bound exceeded.",
        ));
    }
    let mut segments = Vec::with_capacity(count);
    for _ in 0..count {
        let index = reader.u32()?;
        let file_name = std::str::from_utf8(reader.blob(MAX_NAME_BYTES)?)
            .map_err(|_| invalid("Invalid segment filename encoding."))?
            .to_owned();
        let origin_ns = reader.u64()?;
        let encoder_id = std::str::from_utf8(reader.blob(1024)?)
            .map_err(|_| invalid("Invalid encoder identity encoding."))?
            .to_owned();
        let interruption_reason = if reader.flag()? {
            Some(
                std::str::from_utf8(reader.blob(4096)?)
                    .map_err(|_| invalid("Invalid interruption metadata encoding."))?
                    .to_owned(),
            )
        } else {
            None
        };
        let tracks_ebml = reader.blob(MAX_TRACK_HEADER_BYTES)?.to_vec();
        let state = match reader.byte()? {
            0 => SegmentState::Open,
            1 => SegmentState::Interrupted,
            2 => SegmentState::Closed,
            3 => SegmentState::Recovered,
            _ => return Err(invalid("Unknown segment state.")),
        };
        let checkpoint = if reader.flag()? {
            Some(DurableCheckpoint {
                sequence: reader.u64()?,
                position_bytes: reader.u64()?,
                container_end_ns: reader.u64()?,
            })
        } else {
            None
        };
        segments.push(SessionSegment {
            index,
            file_name,
            origin_ns,
            encoder_id,
            interruption_reason,
            tracks_ebml,
            state,
            checkpoint,
            file_bytes: reader.u64()?,
            crc64: reader.u64()?,
            container_end_ns: reader.u64()?,
            cues: reader.u64()?,
        });
    }
    let metadata_file = if version >= 2 && reader.flag()? {
        Some(
            std::str::from_utf8(reader.blob(MAX_NAME_BYTES)?)
                .map_err(|_| invalid("Invalid metadata filename."))?
                .to_owned(),
        )
    } else {
        None
    };
    if !reader.0.is_empty() {
        return Err(invalid("Unexpected manifest tail bytes."));
    }
    let manifest = SessionManifest {
        version,
        generation,
        completed,
        segments,
        metadata_file,
    };
    validate(&manifest)?;
    Ok(manifest)
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn put_blob(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
}
struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if n > self.0.len() {
            return Err(invalid("Manifest was truncated."));
        }
        let (bytes, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(bytes)
    }
    fn byte(&mut self) -> io::Result<u8> {
        Ok(self.bytes(1)?[0])
    }
    fn flag(&mut self) -> io::Result<bool> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(invalid("Invalid manifest flag.")),
        }
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(
            self.bytes(4)?.try_into().expect("four bytes"),
        ))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(
            self.bytes(8)?.try_into().expect("eight bytes"),
        ))
    }
    fn blob(&mut self, limit: usize) -> io::Result<&'a [u8]> {
        let len = self.u32()? as usize;
        if len > limit {
            return Err(invalid("Manifest field exceeds its bound."));
        }
        self.bytes(len)
    }
}

fn atomic_store(path: &Path, manifest: &SessionManifest, replace: bool) -> io::Result<()> {
    let bytes = encode(manifest)?;
    atomic_store_bytes(path, &bytes, manifest.generation, replace)
}

// Replay retirement shares the same sync-before-publication implementation.
fn atomic_store_bytes(path: &Path, bytes: &[u8], generation: u64, replace: bool) -> io::Result<()> {
    let mut temp_name = path.as_os_str().to_os_string();
    temp_name.push(format!(".tmp-{}-{}", std::process::id(), generation));
    let temp = PathBuf::from(temp_name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|error| {
            io::Error::new(error.kind(), format!("Manifest temp creation: {error}"))
        })?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if replace {
            replace_manifest(&temp, path).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("Atomic manifest replacement: {error}"),
                )
            })?;
        } else {
            publish_initial(&temp, path).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("Initial manifest publication: {error}"),
                )
            })?;
        }
        sync_directory(path.parent().expect("validated parent"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Publishes the synced temporary file under its final name.
///
/// Windows renames here because `CreateHardLinkW` followed by deleting the
/// original name leaves the surviving name intermittently un-replaceable: the
/// next `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` returns `ERROR_ACCESS_DENIED`
/// for hundreds of milliseconds or longer even though no process holds the file
/// and it can still be opened with `DELETE` access. Every manifest is published
/// this way, so that state always hit its first checkpoint replacement. The
/// rename is still atomic and crash-safe, and the `create_new` journal marker
/// created before every publication already excludes a competing writer.
#[cfg(windows)]
fn publish_initial(temp: &Path, path: &Path) -> io::Result<()> {
    fs::rename(temp, path)
}

/// Atomic and exclusive on these platforms; the Windows replacement defect does
/// not apply, so the hard-link publication stays unchanged.
#[cfg(not(windows))]
fn publish_initial(temp: &Path, path: &Path) -> io::Result<()> {
    fs::hard_link(temp, path)?;
    fs::remove_file(temp)
}

#[cfg(windows)]
fn replace_manifest(temp: &Path, path: &Path) -> io::Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
    loop {
        match fs::rename(temp, path) {
            Ok(()) => return Ok(()),
            Err(error) => {
                // A Windows reader without delete sharing can briefly block an
                // otherwise valid atomic replacement. Never unlink the previous
                // checkpoint, retry other errors, or hide a persistent denial.
                if matches!(error.raw_os_error(), Some(5 | 32))
                    && std::time::Instant::now() < deadline
                {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                }
                return Err(error);
            }
        }
    }
}

#[cfg(not(windows))]
fn replace_manifest(temp: &Path, path: &Path) -> io::Result<()> {
    fs::rename(temp, path)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}
#[cfg(not(unix))]
fn sync_directory(_: &Path) -> io::Result<()> {
    Ok(())
} // std cannot sync a Windows directory; acceptance remains explicit.

fn file_integrity(path: &Path) -> io::Result<(u64, u64)> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(invalid("Segment is not a regular file."));
    }
    let mut file = File::open(path)?;
    let mut crc = 0;
    let mut size = 0u64;
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        crc = extend_checksum(crc, &buffer[..count]);
        size = size
            .checked_add(count as u64)
            .ok_or_else(|| invalid("Segment length overflow."))?;
    }
    Ok((size, crc))
}

fn checksum(bytes: &[u8]) -> u64 {
    extend_checksum(0, bytes)
}
fn extend_checksum(mut crc: u64, bytes: &[u8]) -> u64 {
    // CRC-64/ECMA-182 (check value for "123456789": 6c40df5f0b497347).
    static TABLE: std::sync::OnceLock<[u64; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        std::array::from_fn(|index| {
            let mut value = (index as u64) << 56;
            for _ in 0..8 {
                value = (value << 1)
                    ^ if value & (1 << 63) != 0 {
                        0x42f0_e1eb_a9ea_3693
                    } else {
                        0
                    };
            }
            value
        })
    });
    for byte in bytes {
        crc = (crc << 8) ^ table[((crc >> 56) as u8 ^ byte) as usize];
    }
    crc
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn session_error(error: io::Error) -> RecorderError {
    RecorderError {
        code: ErrorCode::FinalizationFailed,
        message: format!("Recording session index: {error}"),
        action: "Preserve the original segments and choose a new session manifest path.".into(),
        recovery: RecoveryStatus::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn atomic_manifest_replacement_tolerates_a_short_windows_reader_lock() {
        use std::os::windows::fs::OpenOptionsExt;
        let directory = std::env::temp_dir().join(format!(
            "freym-manifest-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("session");
        atomic_store_bytes(&path, b"previous", 0, false).unwrap();
        let reader = OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)
            .unwrap();
        let blocked = directory.join("blocked");
        fs::write(&blocked, b"check").unwrap();
        let error = fs::rename(&blocked, &path).unwrap_err();
        assert!(matches!(error.raw_os_error(), Some(5 | 32)), "{error}");
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(25));
            drop(reader);
        });
        atomic_store_bytes(&path, b"replacement", 1, true).unwrap();
        release.join().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
    }

    #[cfg(windows)]
    #[test]
    fn persistent_windows_reader_lock_preserves_the_previous_checkpoint() {
        use std::os::windows::fs::OpenOptionsExt;
        let directory = std::env::temp_dir().join(format!(
            "freym-manifest-denial-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("session");
        atomic_store_bytes(&path, b"previous", 0, false).unwrap();
        let _reader = OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)
            .unwrap();
        let error = atomic_store_bytes(&path, b"replacement", 1, true).unwrap_err();
        assert!(error.to_string().contains("Atomic manifest replacement"));
        assert_eq!(fs::read(&path).unwrap(), b"previous");
        assert_eq!(
            fs::read_dir(&directory).unwrap().count(),
            1,
            "temporary publication is removed"
        );
    }

    #[test]
    fn a_valid_checksum_cannot_hide_an_invalid_live_segment_sequence() {
        let segment = |index, state| SessionSegment {
            index,
            file_name: format!("segment-{index}.mkv"),
            origin_ns: u64::from(index) * 20_000_000,
            encoder_id: "fixture".into(),
            interruption_reason: None,
            tracks_ebml: vec![0x16, 0x54, 0xae, 0x6b],
            state,
            checkpoint: None,
            file_bytes: 0,
            crc64: 0,
            container_end_ns: 0,
            cues: 0,
        };
        let manifest = SessionManifest {
            version: MANIFEST_VERSION,
            generation: 7,
            completed: false,
            metadata_file: None,
            segments: vec![
                segment(0, SegmentState::Interrupted),
                segment(1, SegmentState::Open),
            ],
        };
        let mut bytes = encode(&manifest).unwrap();
        // Locate the first state through the actual wire fields, then inject a
        // second live entry and recompute the integrity digest. Schema validation
        // must still reject this, rather than trusting CRC alone.
        let mut reader = Reader(&bytes);
        reader.bytes(25).unwrap(); // magic, version, generation, completion, count
        reader.u32().unwrap();
        reader.blob(MAX_NAME_BYTES).unwrap();
        reader.u64().unwrap();
        reader.blob(1024).unwrap();
        assert!(!reader.flag().unwrap());
        reader.blob(MAX_TRACK_HEADER_BYTES).unwrap();
        let state_offset = bytes.len() - reader.0.len();
        bytes[state_offset] = 0;
        let end = bytes.len() - 8;
        let crc = checksum(&bytes[..end]);
        bytes[end..].copy_from_slice(&crc.to_le_bytes());
        let error = decode(&bytes).unwrap_err();
        assert!(error.to_string().contains("final segment"));
        assert_eq!(decode(&encode(&manifest).unwrap()).unwrap(), manifest);
    }
}
