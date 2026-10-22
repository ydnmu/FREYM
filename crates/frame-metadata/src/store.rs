//! Length + CRC32 framed JSON records. A damaged/truncated tail never destroys
//! preceding records; strict import rejects it, explicit recovery exports a prefix.
use crate::schema::*;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;

pub(crate) const MAGIC: &[u8; 8] = b"FREYME01";

pub(crate) fn crc(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

pub(crate) fn encode(record: &Record) -> io::Result<Vec<u8>> {
    let data = serde_json::to_vec(record)?;
    if data.len() > MAX_RECORD_BYTES {
        return Err(invalid("Metadata record exceeds byte bound."));
    }
    let mut bytes = Vec::with_capacity(data.len() + 8);
    bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&crc(&data).to_le_bytes());
    bytes.extend_from_slice(&data);
    Ok(bytes)
}

fn local_output(path: &Path) -> io::Result<()> {
    if !path.is_absolute() || !path.parent().is_some_and(Path::is_dir) {
        return Err(invalid(
            "Choose a new absolute local metadata path in an existing folder.",
        ));
    }
    #[cfg(target_os = "windows")]
    if !matches!(path.components().next(), Some(std::path::Component::Prefix(prefix))
        if matches!(prefix.kind(), std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)))
    {
        return Err(invalid("Metadata requires a local disk path."));
    }
    Ok(())
}

pub(crate) fn create(path: &Path) -> io::Result<File> {
    local_output(path)?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)?;
    file.try_lock().map_err(io::Error::other)?;
    file.write_all(MAGIC)?;
    Ok(file)
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImportSummary {
    pub events: u64,
    pub completed: bool,
    pub dropped: u64,
    /// True only for a structurally valid End; `completed` separately describes
    /// original success. Missing or damaged tails remain false after recovery.
    pub clean_end: bool,
    pub valid_bytes: u64,
}

/// Streams a bounded file with bounded per-record allocation. Refuses a live
/// writer and nonregular/symlink inputs. Callback receives only validated records.
pub fn read_sidecar(
    path: &Path,
    mut visit: impl FnMut(&Record) -> io::Result<()>,
) -> io::Result<ImportSummary> {
    scan(path, false, &mut visit)
}

fn scan(
    path: &Path,
    prefix: bool,
    visit: &mut impl FnMut(&Record) -> io::Result<()>,
) -> io::Result<ImportSummary> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(invalid("Metadata is not a regular file."));
    }
    let file = File::open(path)?;
    file.try_lock().map_err(io::Error::other)?;
    if file.metadata()?.len() > MAX_SIDECAR_BYTES {
        return Err(invalid("Metadata file exceeds byte bound."));
    }
    let mut reader = BufReader::new(file);
    let mut magic = [0; 8];
    reader.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(invalid("Unknown metadata header."));
    }
    let mut state = Validator::default();
    let mut summary = ImportSummary {
        valid_bytes: 8,
        ..Default::default()
    };
    loop {
        let record = (|| {
            if reader.fill_buf()?.is_empty() {
                return Ok(None);
            }
            let mut frame = [0; 8];
            reader.read_exact(&mut frame)?;
            let size = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
            if size == 0 || size > MAX_RECORD_BYTES {
                return Err(invalid("Invalid metadata frame size."));
            }
            let mut data = vec![0; size];
            reader.read_exact(&mut data)?;
            if crc(&data) != u32::from_le_bytes(frame[4..].try_into().unwrap()) {
                return Err(invalid("Metadata frame CRC mismatch."));
            }
            let record: Record = serde_json::from_slice(&data)?;
            state.accept(&record)?;
            Ok(Some((record, size as u64 + 8)))
        })();
        match record {
            Ok(Some((record, size))) => {
                visit(&record)?;
                summary.valid_bytes += size;
                summary.events = state.events;
                if let Record::End {
                    completed, dropped, ..
                } = record
                {
                    summary.completed = completed;
                    summary.dropped = dropped;
                    summary.clean_end = true;
                } else if let Record::Checkpoint { dropped, .. } = record {
                    summary.dropped = dropped;
                }
            }
            Ok(None) => break,
            Err(_) if prefix && state.header.is_some() => {
                summary.completed = false;
                summary.clean_end = false;
                break;
            }
            Err(error) => return Err(error),
        }
    }
    if state.header.is_none() {
        return Err(invalid("Missing metadata schema."));
    }
    Ok(summary)
}

/// JSONL interchange uses the same typed/versioned records; one record per line.
pub fn export_jsonl(source: &Path, output: &Path) -> io::Result<ImportSummary> {
    local_output(output)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    let result = read_sidecar(source, |record| {
        serde_json::to_writer(&mut file, record)?;
        file.write_all(b"\n")
    });
    finish_output(file, output, result)
}

pub fn import_jsonl(source: &Path, output: &Path) -> io::Result<ImportSummary> {
    if !fs::symlink_metadata(source)?.is_file() || fs::metadata(source)?.len() > MAX_SIDECAR_BYTES {
        return Err(invalid("Invalid metadata import file."));
    }
    let input = File::open(source)?;
    input.try_lock().map_err(io::Error::other)?;
    let mut reader = BufReader::new(input);
    let mut file = create(output)?;
    let result = (|| {
        let mut validator = Validator::default();
        let mut summary = ImportSummary {
            valid_bytes: 8,
            ..Default::default()
        };
        loop {
            let mut line = Vec::new();
            let count = reader
                .by_ref()
                .take((MAX_RECORD_BYTES + 2) as u64)
                .read_until(b'\n', &mut line)?;
            if count == 0 {
                break;
            }
            if line.len() > MAX_RECORD_BYTES + 1 || line.last() != Some(&b'\n') {
                return Err(invalid("Oversized or partial JSONL line."));
            }
            let record: Record = serde_json::from_slice(&line)?;
            validator.accept(&record)?;
            let bytes = encode(&record)?;
            summary.valid_bytes += bytes.len() as u64;
            if summary.valid_bytes > MAX_SIDECAR_BYTES {
                return Err(invalid("Metadata import exceeds byte bound."));
            }
            file.write_all(&bytes)?;
            if let Record::End {
                completed, dropped, ..
            } = record
            {
                summary.completed = completed;
                summary.dropped = dropped;
                summary.clean_end = true;
            } else if let Record::Checkpoint { dropped, .. } = record {
                summary.dropped = dropped;
            }
        }
        if validator.header.is_none() {
            return Err(invalid("Missing metadata header."));
        }
        summary.events = validator.events;
        Ok(summary)
    })();
    finish_output(file, output, result)
}

/// Copies validated frames only, never modifies the original. Missing/damaged
/// final records remain explicitly incomplete; no fabricated completion marker.
pub fn recover_sidecar(source: &Path, output: &Path) -> io::Result<ImportSummary> {
    let mut file = create(output)?;
    // Defer End until the entire scan has established whether trailing bytes are
    // valid. A corrupt tail after an otherwise valid End must not become a clean
    // completed recording simply by copying that End into a shorter file.
    let mut end = None;
    let result = scan(source, true, &mut |record| {
        if matches!(record, Record::End { .. }) {
            end = Some(record.clone());
            Ok(())
        } else {
            file.write_all(&encode(record)?)
        }
    })
    .and_then(|summary| {
        if summary.clean_end
            && let Some(record) = end
        {
            file.write_all(&encode(&record)?)?;
        }
        Ok(summary)
    });
    finish_output(file, output, result)
}

fn finish_output(
    file: File,
    output: &Path,
    result: io::Result<ImportSummary>,
) -> io::Result<ImportSummary> {
    let result = result.and_then(|summary| {
        file.sync_all()?;
        Ok(summary)
    });
    drop(file);
    if result.is_err() {
        let _ = fs::remove_file(output);
    }
    result
}
