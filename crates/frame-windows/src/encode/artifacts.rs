//! Proof artifact writer for the FRAME-0104 encoder example.
//!
//! This is diagnostic proof tooling, not a recorder and not a container writer: it
//! creates exactly one Annex-B elementary stream file and one streaming JSONL sidecar,
//! both exclusively, and it fails the run instead of hiding an incomplete artifact. There
//! is no mux, no index, no recovery journal and no audio.
//!
//! The two rules that matter for honest evidence live here:
//!
//! - **No overwrite.** Both paths are created with `create_new`, so an existing file is
//!   an explicit `InvalidOutputPath` error rather than silently replaced evidence.
//! - **A failed write is a failed run.** Every write and flush error is reported with
//!   the reason, the artifact that failed and the bytes written so far, and the artifacts
//!   stay on disk as explicitly incomplete: nothing is reported as a complete proof
//!   after a failure.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus};

/// One proof artifact pair: the Annex-B stream and its diagnostic sidecar.
pub struct ProofArtifacts {
    stream: Box<dyn Write + Send>,
    sidecar: Box<dyn Write + Send>,
    stream_path: PathBuf,
    sidecar_path: PathBuf,
    stream_bytes: u64,
    sidecar_lines: u64,
    complete: bool,
}

impl std::fmt::Debug for ProofArtifacts {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProofArtifacts")
            .field("stream_path", &self.stream_path)
            .field("sidecar_path", &self.sidecar_path)
            .field("stream_bytes", &self.stream_bytes)
            .field("sidecar_lines", &self.sidecar_lines)
            .field("complete", &self.complete)
            .finish()
    }
}

impl ProofArtifacts {
    /// Creates a new stream at `stream_path` and a sidecar at `<stream_path>.jsonl`.
    ///
    /// Both are created exclusively: an existing file is refused instead of overwritten.
    pub fn create(stream_path: &Path) -> Result<Self, RecorderError> {
        let mut sidecar_path = stream_path.as_os_str().to_owned();
        sidecar_path.push(".jsonl");
        let sidecar_path = PathBuf::from(sidecar_path);
        let stream = open_exclusive(stream_path)?;
        let sidecar = match open_exclusive(&sidecar_path) {
            Ok(sidecar) => sidecar,
            Err(error) => {
                // The stream file was created, so it stays as an explicitly incomplete
                // artifact rather than being removed silently.
                return Err(RecorderError {
                    code: error.code,
                    message: format!(
                        "{} The stream file {} was created first and is left in place as an incomplete artifact.",
                        error.message,
                        stream_path.display()
                    ),
                    recovery: RecoveryStatus::NoRecording,
                    action: error.action,
                });
            }
        };
        Ok(Self {
            stream: Box::new(stream),
            sidecar: Box::new(sidecar),
            stream_path: stream_path.to_path_buf(),
            sidecar_path,
            stream_bytes: 0,
            sidecar_lines: 0,
            complete: false,
        })
    }

    /// One artifact pair over caller-provided writers, for deterministic failure tests.
    #[cfg(test)]
    pub(crate) fn from_writers(
        stream: Box<dyn Write + Send>,
        sidecar: Box<dyn Write + Send>,
        stream_path: PathBuf,
        sidecar_path: PathBuf,
    ) -> Self {
        Self {
            stream,
            sidecar,
            stream_path,
            sidecar_path,
            stream_bytes: 0,
            sidecar_lines: 0,
            complete: false,
        }
    }

    pub fn stream_path(&self) -> &Path {
        &self.stream_path
    }

    pub fn sidecar_path(&self) -> &Path {
        &self.sidecar_path
    }

    /// Bytes written to the stream so far: the offset of the next access unit.
    pub fn stream_offset(&self) -> u64 {
        self.stream_bytes
    }

    pub fn sidecar_lines(&self) -> u64 {
        self.sidecar_lines
    }

    /// True only after [`ProofArtifacts::finish`] flushed and closed both artifacts.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// Writes the codec initialization bytes (Annex-B SPS/PPS) at the head of the stream.
    pub fn write_initialization(&mut self, bytes: &[u8]) -> Result<(), RecorderError> {
        self.write_stream(bytes)
    }

    /// Appends one complete access unit and returns its offset in the stream.
    pub fn write_access_unit(&mut self, bytes: &[u8]) -> Result<u64, RecorderError> {
        let offset = self.stream_bytes;
        self.write_stream(bytes)?;
        Ok(offset)
    }

    /// Appends one diagnostic line to the streaming sidecar.
    pub fn write_sidecar_line(&mut self, line: &str) -> Result<(), RecorderError> {
        let mut buffer = String::with_capacity(line.len() + 1);
        buffer.push_str(line);
        buffer.push('\n');
        if let Err(error) = self.sidecar.write_all(buffer.as_bytes()) {
            return Err(write_error(
                "diagnostic sidecar",
                &self.sidecar_path,
                &error,
            ));
        }
        if let Err(error) = self.sidecar.flush() {
            return Err(write_error(
                "diagnostic sidecar",
                &self.sidecar_path,
                &error,
            ));
        }
        self.sidecar_lines += 1;
        Ok(())
    }

    /// Flushes and closes both artifacts, marking the run complete.
    pub fn finish(mut self) -> Result<(), RecorderError> {
        self.stream
            .flush()
            .map_err(|error| write_error("Annex-B stream", &self.stream_path, &error))?;
        self.sidecar
            .flush()
            .map_err(|error| write_error("diagnostic sidecar", &self.sidecar_path, &error))?;
        self.complete = true;
        Ok(())
    }

    fn write_stream(&mut self, bytes: &[u8]) -> Result<(), RecorderError> {
        if let Err(error) = self.stream.write_all(bytes) {
            return Err(write_error("Annex-B stream", &self.stream_path, &error));
        }
        self.stream_bytes += bytes.len() as u64;
        Ok(())
    }
}

/// Opens one path exclusively, refusing to overwrite existing evidence.
fn open_exclusive(path: &Path) -> Result<File, RecorderError> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            let detail = if error.kind() == std::io::ErrorKind::AlreadyExists {
                "already exists, and this proof never overwrites evidence".to_string()
            } else {
                format!("could not be created: {error}")
            };
            RecorderError {
                code: ErrorCode::InvalidOutputPath,
                message: format!("The proof artifact {} {detail}.", path.display()),
                recovery: RecoveryStatus::NoRecording,
                action: "Choose a new output path under the ignored test-results/ directory."
                    .to_string(),
            }
        })
}

/// Maps a write or flush failure onto the accepted taxonomy, keeping the partial state.
fn write_error(artifact: &str, path: &Path, error: &std::io::Error) -> RecorderError {
    let code = if error.kind() == std::io::ErrorKind::StorageFull {
        ErrorCode::DiskFull
    } else {
        ErrorCode::InvalidOutputPath
    };
    RecorderError {
        code,
        message: format!(
            "Writing the {artifact} artifact {} failed: {error}. The artifact is incomplete and must not be reported as a finished proof.",
            path.display()
        ),
        recovery: RecoveryStatus::Unknown,
        action: "Free space or choose another path, then run the proof again; do not treat the partial file as evidence."
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    /// A writer that fails after a fixed number of successful writes.
    struct FailingWriter {
        accept: usize,
        written: usize,
    }

    impl Write for FailingWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if self.written >= self.accept {
                return Err(io::Error::other("SCRIPTED write failure"));
            }
            self.written += 1;
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("SCRIPTED flush failure"))
        }
    }

    struct Sink;

    impl Write for Sink {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "frame-0104-artifacts-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        path
    }

    #[test]
    fn artifacts_are_created_exclusively_and_refuse_overwrites() {
        let stream = scratch("overwrite");
        let artifacts = ProofArtifacts::create(&stream).expect("the first creation succeeds");
        assert_eq!(
            artifacts.sidecar_path(),
            PathBuf::from(format!("{}.jsonl", stream.display()))
        );
        assert!(!artifacts.is_complete());
        // A second creation over the same path is refused, and the refusal names the file.
        let error = ProofArtifacts::create(&stream).expect_err("an existing artifact is refused");
        assert_eq!(error.code, ErrorCode::InvalidOutputPath);
        assert!(
            error.message.contains("already exists"),
            "{}",
            error.message
        );
        let sidecar = artifacts.sidecar_path().to_path_buf();
        artifacts.finish().expect("the first pair finishes");
        let _ = std::fs::remove_file(&stream);
        let _ = std::fs::remove_file(sidecar);
    }

    #[test]
    fn a_write_failure_fails_the_run_and_never_looks_complete() {
        let stream = scratch("write-failure-stream");
        let sidecar = scratch("write-failure-sidecar");
        let mut artifacts = ProofArtifacts::from_writers(
            Box::new(FailingWriter {
                accept: 1,
                written: 0,
            }),
            Box::new(Sink),
            stream,
            sidecar,
        );
        artifacts
            .write_access_unit(&[0u8; 16])
            .expect("the first write is accepted");
        assert_eq!(artifacts.stream_offset(), 16);
        let error = artifacts
            .write_access_unit(&[0u8; 16])
            .expect_err("the second write fails");
        assert!(error.message.contains("incomplete"), "{}", error.message);
        assert!(!artifacts.is_complete());
    }

    #[test]
    fn a_sidecar_failure_is_reported_with_its_own_artifact() {
        let stream = scratch("sidecar-failure-stream");
        let sidecar = scratch("sidecar-failure-sidecar");
        let mut artifacts = ProofArtifacts::from_writers(
            Box::new(Sink),
            Box::new(FailingWriter {
                accept: 0,
                written: 0,
            }),
            stream,
            sidecar,
        );
        let error = artifacts
            .write_sidecar_line("{\"kind\":\"settings\"}")
            .expect_err("the sidecar write fails");
        assert!(
            error.message.contains("diagnostic sidecar"),
            "{}",
            error.message
        );
        assert_eq!(artifacts.sidecar_lines(), 0);
    }

    #[test]
    fn a_flush_failure_fails_the_run_instead_of_reporting_a_complete_proof() {
        let stream = scratch("flush-failure-stream");
        let sidecar = scratch("flush-failure-sidecar");
        let artifacts = ProofArtifacts::from_writers(
            Box::new(FailingWriter {
                accept: usize::MAX,
                written: 0,
            }),
            Box::new(Sink),
            stream,
            sidecar,
        );
        let error = artifacts.finish().expect_err("the flush failure surfaces");
        assert!(error.message.contains("incomplete"), "{}", error.message);
    }

    #[test]
    fn offsets_and_line_counts_are_tracked_exactly() {
        let stream = scratch("offsets-stream");
        let sidecar = scratch("offsets-sidecar");
        let mut artifacts =
            ProofArtifacts::from_writers(Box::new(Sink), Box::new(Sink), stream, sidecar);
        artifacts.write_initialization(&[0, 0, 0, 1]).expect("init");
        assert_eq!(artifacts.stream_offset(), 4);
        let offset = artifacts.write_access_unit(&[1, 2, 3]).expect("unit");
        assert_eq!(offset, 4);
        assert_eq!(artifacts.stream_offset(), 7);
        artifacts.write_sidecar_line("{}").expect("line");
        artifacts.write_sidecar_line("{}").expect("line");
        assert_eq!(artifacts.sidecar_lines(), 2);
        let complete = artifacts.finish().is_ok();
        assert!(complete);
    }
}
