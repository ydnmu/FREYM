//! Persistent evidence of recordings that did not complete durable finalization.
//!
//! A small sibling file is synced before container headers are written and held
//! exclusively locked for the writer's lifetime. Only successful finalization
//! removes it. Discovery never reads media, follows subdirectories or offers a
//! marker still owned by a live writer. This is restart detection, not a promise
//! that every interrupted file contains recoverable packets or survived power loss.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const SUFFIX: &str = ".freym-incomplete";
const MAGIC: &[u8] = b"FREYM recording journal v1\n";
pub const MAX_INTERRUPTED_RECORDINGS: usize = 32;
const MAX_DIRECTORY_ENTRIES: usize = 4096;

pub(crate) struct RecordingJournal {
    path: PathBuf,
    // Keeping this handle alive keeps the lock alive, including during finalize.
    _file: File,
}

impl RecordingJournal {
    pub(crate) fn create(recording: &Path) -> io::Result<Self> {
        let path = marker_path(recording);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.try_lock()?;
        file.write_all(MAGIC)?;
        file.sync_all()?;
        Ok(Self { path, _file: file })
    }

    /// Called only after the recording itself was flushed, synced and closed.
    /// Keep the lock until removal finishes so discovery cannot race finalization.
    pub(crate) fn complete(self) -> io::Result<()> {
        fs::remove_file(&self.path)
    }
}

fn marker_path(recording: &Path) -> PathBuf {
    let mut name = recording.as_os_str().to_os_string();
    name.push(SUFFIX);
    PathBuf::from(name)
}

/// Bounded candidates in one recording folder. `truncated` means callers must
/// offer manual selection too. Missing folders have no recordings to recover.
pub fn interrupted_recordings(directory: &Path) -> io::Result<(Vec<PathBuf>, bool)> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), false)),
        Err(error) => return Err(error),
    };
    let mut paths = Vec::new();
    let mut truncated = false;
    for (index, entry) in entries.enumerate() {
        if index >= MAX_DIRECTORY_ENTRIES {
            truncated = true;
            break;
        }
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(source_name) = name.to_str().and_then(|name| name.strip_suffix(SUFFIX)) else {
            continue;
        };
        if source_name.is_empty() {
            continue;
        }
        let mut marker = match OpenOptions::new().read(true).write(true).open(entry.path()) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        match marker.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => continue,
            Err(TryLockError::Error(error)) => return Err(error),
        }
        if marker.metadata()?.len() != MAGIC.len() as u64 {
            continue;
        }
        let mut bytes = [0; MAGIC.len()];
        marker.read_exact(&mut bytes)?;
        if bytes != MAGIC {
            continue;
        }
        let source = directory.join(source_name);
        match fs::symlink_metadata(&source) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        }
        if paths.len() == MAX_INTERRUPTED_RECORDINGS {
            truncated = true;
            break;
        }
        paths.push(source);
    }
    paths.sort();
    Ok((paths, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "freym-journal-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn live_writer_is_excluded_and_an_abandoned_journal_is_discovered() {
        let scratch = Scratch::new();
        let source = scratch.0.join("テスト recording.mkv");
        fs::write(&source, b"preserved media").unwrap();
        let journal = RecordingJournal::create(&source).unwrap();
        assert_eq!(interrupted_recordings(&scratch.0).unwrap(), (vec![], false));
        drop(journal);
        assert_eq!(
            interrupted_recordings(&scratch.0).unwrap(),
            (vec![source.clone()], false)
        );
        assert_eq!(fs::read(source).unwrap(), b"preserved media");
    }

    #[test]
    fn completion_removes_the_marker_while_still_owning_its_lock() {
        let scratch = Scratch::new();
        let source = scratch.0.join("finished.mkv");
        fs::write(&source, b"finished media").unwrap();
        RecordingJournal::create(&source)
            .unwrap()
            .complete()
            .unwrap();
        assert!(!marker_path(&source).exists());
        assert_eq!(interrupted_recordings(&scratch.0).unwrap(), (vec![], false));
    }

    #[test]
    fn unknown_markers_or_missing_sources_are_not_offered() {
        let scratch = Scratch::new();
        let source = scratch.0.join("wrong.mkv");
        fs::write(&source, b"media").unwrap();
        fs::write(marker_path(&source), b"unrecognized marker").unwrap();
        drop(RecordingJournal::create(&scratch.0.join("missing.mkv")).unwrap());
        fs::create_dir(scratch.0.join("subdirectory")).unwrap();
        assert_eq!(interrupted_recordings(&scratch.0).unwrap(), (vec![], false));
    }

    #[test]
    fn candidates_are_bounded_and_report_truncation() {
        let scratch = Scratch::new();
        for index in 0..=MAX_INTERRUPTED_RECORDINGS {
            let source = scratch.0.join(format!("recording-{index}.mkv"));
            fs::write(&source, b"media").unwrap();
            drop(RecordingJournal::create(&source).unwrap());
        }
        let (paths, truncated) = interrupted_recordings(&scratch.0).unwrap();
        assert_eq!(paths.len(), MAX_INTERRUPTED_RECORDINGS);
        assert!(truncated);
    }

    #[test]
    fn an_existing_marker_is_never_overwritten() {
        let scratch = Scratch::new();
        let source = scratch.0.join("existing.mkv");
        fs::write(marker_path(&source), b"old evidence").unwrap();
        assert!(RecordingJournal::create(&source).is_err());
        assert_eq!(fs::read(marker_path(&source)).unwrap(), b"old evidence");
    }
}
