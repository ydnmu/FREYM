//! Only explicitly owned replay archives may delete a retired, verified prefix.
//! The existing session wire codec and atomic publication bind a retirement to
//! one exact manifest transition; CRC is corruption detection, not authentication.
use super::*;

const OWNER_NAME: &str = ".freym-replay-owned";
const OWNER_MAGIC: &[u8] = b"FREYM owned replay v1\nreplay.freym-session\n";
const MANIFEST_NAME: &str = "replay.freym-session";
const INTENT_NAME: &str = ".freym-retirement";
const INTENT_MAGIC: &[u8; 8] = b"FREYMRT1";
const MAX_INTENT_BYTES: usize = MAX_SESSION_MANIFEST_BYTES + 24;

/// Immutable content; keeping this file handle locked excludes offline cleanup
/// while the recording owner is sealing, rotating, or exporting clips.
pub(crate) struct ArchiveOwnership {
    _file: File,
}

impl ArchiveOwnership {
    pub(crate) fn create(directory: &Path) -> io::Result<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(directory.join(OWNER_NAME))?;
        file.try_lock().map_err(io::Error::other)?;
        file.write_all(&owner_bytes())?;
        file.sync_all()?;
        sync_directory(directory)?;
        Ok(Self { _file: file })
    }

    fn open(directory: &Path) -> io::Result<Self> {
        regular_metadata(directory.join(OWNER_NAME).as_path())?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(directory.join(OWNER_NAME))?;
        file.try_lock().map_err(|error| {
            io::Error::other(format!(
                "Replay archive is still owned or cannot be locked: {error}"
            ))
        })?;
        let expected = owner_bytes();
        if file.metadata()?.len() != expected.len() as u64 {
            return Err(invalid(
                "Unknown replay ownership marker; all files preserved.",
            ));
        }
        let mut actual = vec![0; expected.len()];
        file.read_exact(&mut actual)?;
        if actual != expected {
            return Err(invalid(
                "Unknown replay ownership marker; all files preserved.",
            ));
        }
        Ok(Self { _file: file })
    }
}

fn owner_bytes() -> Vec<u8> {
    let mut bytes = OWNER_MAGIC.to_vec();
    put_u64(&mut bytes, checksum(OWNER_MAGIC));
    bytes
}

struct Intent {
    original: SessionManifest,
    count: usize,
}

impl Intent {
    fn next(&self) -> io::Result<SessionManifest> {
        if self.count == 0
            || self.count > self.original.segments.len()
            || self.original.completed
            || self
                .original
                .segments
                .iter()
                .any(|s| s.state != SegmentState::Closed)
        {
            return Err(invalid(
                "Retirement intent does not contain a sealed replay prefix.",
            ));
        }
        let mut next = self.original.clone();
        next.generation = next
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("Manifest generation exhausted."))?;
        next.segments.drain(..self.count);
        for (index, segment) in next.segments.iter_mut().enumerate() {
            segment.index = index as u32;
        }
        validate(&next)?;
        Ok(next)
    }

    fn encode(&self) -> io::Result<Vec<u8>> {
        self.next()?;
        let mut bytes = INTENT_MAGIC.to_vec();
        put_u32(&mut bytes, self.count as u32);
        put_blob(&mut bytes, &encode(&self.original)?);
        let crc = checksum(&bytes);
        put_u64(&mut bytes, crc);
        Ok(bytes)
    }

    fn read(path: &Path) -> io::Result<Self> {
        let meta = regular_metadata(path)?;
        if meta.len() > MAX_INTENT_BYTES as u64 || meta.len() < 24 {
            return Err(invalid("Retirement intent is not bounded."));
        }
        let mut bytes = Vec::with_capacity(meta.len() as usize);
        File::open(path)?
            .take(MAX_INTENT_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_INTENT_BYTES {
            return Err(invalid("Retirement intent grew beyond its bound."));
        }
        let (body, crc) = bytes.split_at(bytes.len() - 8);
        if checksum(body) != u64::from_le_bytes(crc.try_into().expect("eight bytes")) {
            return Err(invalid(
                "Retirement intent integrity check failed; files preserved.",
            ));
        }
        let mut reader = Reader(body);
        if reader.bytes(8)? != INTENT_MAGIC {
            return Err(invalid("Unknown retirement intent; files preserved."));
        }
        let count = reader.u32()? as usize;
        let original = decode(reader.blob(MAX_SESSION_MANIFEST_BYTES)?)?;
        if !reader.0.is_empty() {
            return Err(invalid("Unexpected retirement intent tail."));
        }
        let intent = Self { original, count };
        intent.next()?;
        Ok(intent)
    }
}

fn owned_names(source: &Path, manifest: &SessionManifest) -> io::Result<()> {
    if source.file_name().and_then(|name| name.to_str()) != Some(MANIFEST_NAME) {
        return Err(invalid(
            "Only the private replay manifest can retire files.",
        ));
    }
    let prefix = format!("freym-{:016x}-rolling-", checksum(MANIFEST_NAME.as_bytes()));
    for segment in &manifest.segments {
        let suffix = segment
            .file_name
            .strip_prefix(&prefix)
            .and_then(|name| name.strip_suffix(".mkv"))
            .ok_or_else(|| invalid("Retirement contains a file outside owned replay naming."))?;
        if suffix.len() != 16 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(invalid(
                "Retirement contains an invalid owned replay filename.",
            ));
        }
    }
    Ok(())
}

pub(super) fn prepare(source: &Path, original: &SessionManifest, count: usize) -> io::Result<()> {
    owned_names(source, original)?;
    let directory = source.parent().expect("validated parent");
    let intent = Intent {
        original: original.clone(),
        count,
    };
    let bytes = intent.encode()?;
    for segment in original.segments.iter().take(count) {
        let path = directory.join(&segment.file_name);
        regular_metadata(&path)?;
        if file_integrity(&path)? != (segment.file_bytes, segment.crc64) {
            return Err(invalid(
                "Replay retirement integrity mismatch; file preserved.",
            ));
        }
    }
    atomic_store_bytes(
        &directory.join(INTENT_NAME),
        &bytes,
        original.generation,
        false,
    )
}

/// The caller already owns either the live session plus archive locks, or both
/// abandoned session journal and archive locks. Validate the entire deletion set
/// before removing anything. Missing files permit resuming a partial deletion.
pub(super) fn cleanup(source: &Path) -> io::Result<usize> {
    let directory = source.parent().expect("validated parent");
    let intent_path = directory.join(INTENT_NAME);
    match fs::symlink_metadata(&intent_path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    let intent = Intent::read(&intent_path)?;
    owned_names(source, &intent.original)?;
    let current = read_manifest(source)?;
    let next = intent.next()?;
    if current == intent.original {
        // Publication never happened: every media reference stays intact.
        fs::remove_file(&intent_path)?;
        sync_directory(directory)?;
        return Ok(0);
    }
    if current != next {
        return Err(invalid(
            "Retirement does not match the exact manifest transition; files preserved.",
        ));
    }
    validate_directory(directory, &intent.original)?;
    let mut deleting = Vec::new();
    for segment in intent.original.segments.iter().take(intent.count) {
        if current
            .segments
            .iter()
            .any(|s| s.file_name.eq_ignore_ascii_case(&segment.file_name))
        {
            return Err(invalid(
                "Retired replay file is still referenced; files preserved.",
            ));
        }
        let path = directory.join(&segment.file_name);
        match regular_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
            Ok(_) => {}
        }
        if file_integrity(&path)? != (segment.file_bytes, segment.crc64) {
            return Err(invalid(
                "Retired replay file no longer matches its durable identity; files preserved.",
            ));
        }
        deleting.push(path);
    }
    let count = deleting.len();
    for path in deleting {
        fs::remove_file(path)?;
    }
    sync_directory(directory)?;
    fs::remove_file(intent_path)?;
    sync_directory(directory)?;
    Ok(count)
}

fn validate_directory(directory: &Path, manifest: &SessionManifest) -> io::Result<()> {
    let journal = format!("{MANIFEST_NAME}.freym-incomplete");
    let events = metadata_path(&directory.join(MANIFEST_NAME));
    let events = events.file_name().and_then(|name| name.to_str());
    for (index, entry) in fs::read_dir(directory)?.enumerate() {
        if index >= MAX_SESSION_SEGMENTS + 6 {
            return Err(invalid(
                "Unexpected replay directory entry count; files preserved.",
            ));
        }
        let entry = entry?;
        regular_metadata(&entry.path())?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| invalid("Unknown replay filename; files preserved."))?;
        if ![OWNER_NAME, MANIFEST_NAME, journal.as_str(), INTENT_NAME].contains(&name)
            && manifest.metadata_file.as_deref() != Some(name)
            // A crash can leave the owned metadata replacement before its manifest
            // reference. Preserve both files without blocking verified media recovery.
            && events != Some(name)
            && name != "events.pending"
            && !manifest
                .segments
                .iter()
                .any(|segment| segment.file_name == name)
        {
            return Err(invalid(
                "Replay directory contains an unknown file; retirement preserved.",
            ));
        }
    }
    Ok(())
}

fn regular_metadata(path: &Path) -> io::Result<fs::Metadata> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() || is_reparse(&meta) {
        return Err(invalid(
            "Replay file is not a regular owned file; files preserved.",
        ));
    }
    Ok(meta)
}

#[cfg(windows)]
fn is_reparse(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes() & 0x400 != 0
}
#[cfg(not(windows))]
fn is_reparse(meta: &fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}

/// Explicit offline cleanup/export only. No recording session or fresh media
/// clock is adopted; ordinary recording folders have no matching ownership marker.
pub(crate) fn recover_owned(
    directory: &Path,
    output: &Path,
) -> Result<SessionRecoverySummary, RecorderError> {
    if !directory.is_absolute() {
        return Err(session_error(invalid(
            "Replay recovery directory must be absolute.",
        )));
    }
    let meta = fs::symlink_metadata(directory).map_err(session_error)?;
    if !meta.is_dir() || is_reparse(&meta) {
        return Err(session_error(invalid(
            "Replay recovery directory is not an owned regular directory.",
        )));
    }
    let directory = directory.canonicalize().map_err(session_error)?;
    let output = absolute_manifest_path(output).map_err(session_error)?;
    if output.parent() == Some(directory.as_path()) || output.exists() {
        return Err(session_error(invalid(
            "Choose a new export manifest outside the owned replay archive.",
        )));
    }
    let _owner = ArchiveOwnership::open(&directory).map_err(session_error)?;
    let source = directory.join(MANIFEST_NAME);
    let _journal = abandoned_session_guard(&source)
        .map_err(session_error)?
        .ok_or_else(|| {
            session_error(invalid(
                "Owned replay recovery requires its abandoned session journal.",
            ))
        })?;
    cleanup(&source).map_err(session_error)?;
    // This private path bypasses re-locking only while both guards above live.
    // It requires all retained segments to be closed and all CRCs to match.
    if read_manifest(&source)
        .map_err(session_error)?
        .segments
        .iter()
        .all(|s| s.state == SegmentState::Closed)
    {
        export_closed_snapshot(&source, &output)
    } else {
        // A process can stop during a background seal. Reuse checkpoint-prefix
        // recovery for that tail while preserving verified earlier segments.
        recover_owned_snapshot(&source, &output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (PathBuf, SessionManifest, ArchiveOwnership, RecordingJournal) {
        let root = std::env::temp_dir().join("freym-retirement");
        fs::create_dir_all(&root).unwrap();
        let directory = root.join(format!(
            "retirement-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let owner = ArchiveOwnership::create(&directory).unwrap();
        let source = directory.join(MANIFEST_NAME);
        let journal = RecordingJournal::create(&source).unwrap();
        let segments = (0..3)
            .map(|index| {
                let data = format!("owned sealed fixture {index}").into_bytes();
                let file_name = format!(
                    "freym-{:016x}-rolling-{index:016x}.mkv",
                    checksum(MANIFEST_NAME.as_bytes())
                );
                fs::write(directory.join(&file_name), &data).unwrap();
                SessionSegment {
                    index,
                    file_name,
                    origin_ns: u64::from(index) * 1_000_000_000,
                    encoder_id: "retirement-fixture".into(),
                    interruption_reason: None,
                    tracks_ebml: vec![0x16, 0x54, 0xae, 0x6b],
                    state: SegmentState::Closed,
                    checkpoint: None,
                    file_bytes: data.len() as u64,
                    crc64: checksum(&data),
                    container_end_ns: 1_000_000_000,
                    cues: 1,
                }
            })
            .collect();
        let manifest = SessionManifest {
            version: MANIFEST_VERSION,
            generation: 31,
            completed: false,
            segments,
            metadata_file: None,
        };
        atomic_store(&source, &manifest, false).unwrap();
        (directory, manifest, owner, journal)
    }

    fn publish_retirement(directory: &Path, original: &SessionManifest, count: usize) {
        let source = directory.join(MANIFEST_NAME);
        prepare(&source, original, count).unwrap();
        let next = Intent::read(&directory.join(INTENT_NAME))
            .unwrap()
            .next()
            .unwrap();
        atomic_store(&source, &next, true).unwrap();
    }

    #[test]
    fn intent_before_publication_never_deletes_referenced_media_and_live_locks_block_recovery() {
        let (directory, original, owner, journal) = fixture();
        let source = directory.join(MANIFEST_NAME);
        prepare(&source, &original, 2).unwrap();
        assert!(recover_owned(&directory, &directory.with_extension("export")).is_err());
        drop(owner);
        // Even if the archive handle is released, a live session excludes cleanup.
        assert!(recover_owned(&directory, &directory.with_extension("export")).is_err());
        drop(journal);
        let _owner = ArchiveOwnership::open(&directory).unwrap();
        let _journal = abandoned_session_guard(&source).unwrap().unwrap();
        assert_eq!(cleanup(&source).unwrap(), 0);
        assert_eq!(read_manifest(&source).unwrap(), original);
        assert!(!directory.join(INTENT_NAME).exists());
        for segment in &original.segments {
            assert_eq!(
                file_integrity(&directory.join(&segment.file_name)).unwrap(),
                (segment.file_bytes, segment.crc64)
            );
        }
        println!(
            "Retirement before publication evidence: {}",
            directory.display()
        );
    }

    #[test]
    fn published_partial_retirement_resumes_only_known_unreferenced_verified_files() {
        let (directory, original, _owner, _journal) = fixture();
        publish_retirement(&directory, &original, 2);
        let source = directory.join(MANIFEST_NAME);
        let unknown = directory.join("ordinary-recording.mkv");
        fs::write(&unknown, b"never owned by rolling replay").unwrap();
        assert!(cleanup(&source).is_err());
        assert!(
            original
                .segments
                .iter()
                .all(|s| directory.join(&s.file_name).exists())
        );
        assert_eq!(
            fs::read(&unknown).unwrap(),
            b"never owned by rolling replay"
        );
        // The fixture explicitly moves the unrelated recording outside the archive.
        fs::rename(&unknown, directory.with_extension("ordinary-mkv")).unwrap();
        fs::remove_file(directory.join(&original.segments[0].file_name)).unwrap();
        assert_eq!(cleanup(&source).unwrap(), 1);
        assert!(!directory.join(&original.segments[1].file_name).exists());
        let retained = &original.segments[2];
        assert_eq!(
            file_integrity(&directory.join(&retained.file_name)).unwrap(),
            (retained.file_bytes, retained.crc64)
        );
        assert!(!directory.join(INTENT_NAME).exists());
        println!("Partial retirement evidence: {}", directory.display());
    }

    #[test]
    fn corrupt_intent_media_and_unexpected_generation_preserve_retirement_evidence() {
        let (directory, original, _owner, _journal) = fixture();
        publish_retirement(&directory, &original, 2);
        let source = directory.join(MANIFEST_NAME);
        let intent_path = directory.join(INTENT_NAME);
        let intent_bytes = fs::read(&intent_path).unwrap();
        let mut corrupt = intent_bytes.clone();
        corrupt[12] ^= 1;
        fs::write(&intent_path, &corrupt).unwrap();
        assert!(cleanup(&source).is_err());
        fs::write(&intent_path, &intent_bytes).unwrap();
        let path = directory.join(&original.segments[1].file_name);
        fs::write(&path, b"corrupted retired clip").unwrap();
        assert!(cleanup(&source).is_err());
        assert!(
            directory.join(&original.segments[0].file_name).exists(),
            "preverify all before first deletion"
        );
        let mut unexpected = read_manifest(&source).unwrap();
        unexpected.generation += 1;
        atomic_store(&source, &unexpected, true).unwrap();
        assert!(cleanup(&source).is_err());
        assert!(
            original
                .segments
                .iter()
                .all(|s| directory.join(&s.file_name).exists())
        );
        assert_eq!(fs::read(&intent_path).unwrap(), intent_bytes);
        let ordinary = directory.with_extension("ordinary");
        fs::create_dir(&ordinary).unwrap();
        fs::write(ordinary.join(MANIFEST_NAME), fs::read(&source).unwrap()).unwrap();
        assert!(recover_owned(&ordinary, &directory.with_extension("export")).is_err());
        println!("Corrupt retirement evidence: {}", directory.display());
    }
}
