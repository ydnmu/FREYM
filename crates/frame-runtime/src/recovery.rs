//! Desktop admission and wire mapping for the existing bounded MKV recovery engine.
use frame_contracts::{
    ErrorCode, InterruptedRecordingScan, RecorderError, RecorderState, RecordingRecoveryResult,
    RecoveryStatus,
};
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Read only discovery. Markers owned by live writers are excluded by the mux
/// journal's nonblocking file lock, even when another app instance is recording.
pub fn discover(directory: &Path) -> Result<InterruptedRecordingScan, RecorderError> {
    if !directory.is_absolute() {
        return Err(refusal(
            ErrorCode::InvalidRequest,
            "Choose an absolute recording folder.",
        ));
    }
    let (paths, truncated) = frame_mux::interrupted_recordings(directory).map_err(|error| {
        refusal(
            ErrorCode::InvalidOutputPath,
            &format!("Could not check the recording folder for interrupted recordings: {error}"),
        )
    })?;
    Ok(InterruptedRecordingScan {
        paths: paths
            .into_iter()
            .filter(|path| {
                path.extension()
                    .is_none_or(|extension| extension != "freym-session")
            })
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        truncated,
    })
}

/// Separate session index discovery keeps manifests out of the single-MKV flow.
pub fn discover_sessions(directory: &Path) -> Result<InterruptedRecordingScan, RecorderError> {
    if !directory.is_absolute() {
        return Err(refusal(
            ErrorCode::InvalidRequest,
            "Choose an absolute recording folder.",
        ));
    }
    let (paths, truncated) = frame_mux::interrupted_recordings(directory)
        .map_err(|error| refusal(ErrorCode::InvalidOutputPath, &error.to_string()))?;
    Ok(InterruptedRecordingScan {
        paths: paths
            .into_iter()
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "freym-session")
            })
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        truncated,
    })
}

/// One bounded product scan, including private replay folders from prior runs.
/// Discovery never locks an archive for cleanup or follows directory links.
pub fn discover_available(directory: &Path) -> Result<InterruptedRecordingScan, RecorderError> {
    let mut scan = discover(directory)?;
    let sessions = discover_sessions(directory)?;
    scan.paths.extend(sessions.paths);
    scan.truncated |= sessions.truncated;
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(scan),
        Err(e) => return Err(refusal(ErrorCode::InvalidOutputPath, &e.to_string())),
    };
    for (index, entry) in entries.enumerate() {
        if index >= 4096 || scan.paths.len() >= 32 {
            scan.truncated = true;
            break;
        }
        let entry = entry.map_err(|e| refusal(ErrorCode::InvalidOutputPath, &e.to_string()))?;
        if !entry.file_type().is_ok_and(|t| t.is_dir())
            || !entry
                .file_name()
                .to_string_lossy()
                .ends_with(".freym-replay")
        {
            continue;
        }
        let path = entry.path();
        // Reject reparse points as well as symlinks on Windows.
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_attributes() & 0x400 != 0) {
                continue;
            }
        }
        for candidate in [&path, &path.join("buffer")] {
            let meta = std::fs::symlink_metadata(candidate);
            if !meta
                .as_ref()
                .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
            {
                continue;
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if meta
                    .as_ref()
                    .is_ok_and(|m| m.file_attributes() & 0x400 != 0)
                {
                    continue;
                }
            }
            if candidate.join(".freym-replay-owned").is_file() {
                let found = discover_sessions(candidate)?;
                scan.paths.extend(found.paths.into_iter().filter(|p| {
                    Path::new(p)
                        .file_name()
                        .is_some_and(|n| n == "replay.freym-session")
                }));
                scan.truncated |= found.truncated;
            }
        }
    }
    scan.paths.sort();
    scan.paths.dedup();
    if scan.paths.len() > 32 {
        scan.paths.truncate(32);
        scan.truncated = true;
    }
    Ok(scan)
}

#[derive(Clone, Default)]
pub struct RecoveryJobs(Arc<AtomicBool>);

pub struct RecoveryJob(Arc<AtomicBool>);

impl RecoveryJobs {
    pub fn begin(&self, state: &RecorderState) -> Result<RecoveryJob, RecorderError> {
        if !matches!(state, RecorderState::Idle | RecorderState::Failed(_)) {
            return Err(refusal(
                ErrorCode::InvalidTransition,
                "Stop the recording before recovering a file.",
            ));
        }
        self.0
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                refusal(
                    ErrorCode::InvalidTransition,
                    "A file recovery is already running.",
                )
            })?;
        Ok(RecoveryJob(Arc::clone(&self.0)))
    }
}

impl RecoveryJob {
    pub fn run_any(
        self,
        source: &Path,
        output: &Path,
    ) -> Result<RecordingRecoveryResult, RecorderError> {
        if source.extension().is_none_or(|ext| ext != "freym-session") {
            return self.run(source, output);
        }
        if !source.is_absolute()
            || !output.is_absolute()
            || output.extension().is_none_or(|e| e != "freym-session")
        {
            return Err(refusal(
                ErrorCode::InvalidRequest,
                "Choose absolute session paths and a new .freym-session destination.",
            ));
        }
        let summary = if source
            .file_name()
            .is_some_and(|n| n == "replay.freym-session")
            && source
                .parent()
                .is_some_and(|d| d.join(".freym-replay-owned").exists())
        {
            frame_mux::replay::RollingReplayStore::recover_owned(source.parent().unwrap(), output)?
        } else {
            self.run_session(source, output)?
        };
        let mut notes: Vec<String> = summary
            .manifest
            .segments
            .iter()
            .map(|s| {
                format!(
                    "Playable segment: {}",
                    output.parent().unwrap().join(&s.file_name).display()
                )
            })
            .chain(
                summary
                    .rejected_segments
                    .iter()
                    .map(|(name, reason)| format!("Preserved rejected segment {name}: {reason}")),
            )
            .collect();
        if let Some(metadata) = &summary.metadata_recovery {
            notes.push(format!("Interaction metadata: {} validated events, {} dropped; clean end={}, original completion={}. Timeline uses the preserved segment origins.",
                metadata.events, metadata.dropped, metadata.clean_end, metadata.completed));
        }
        if let Some(error) = &summary.metadata_error {
            notes.push(format!("Interaction metadata could not be recovered: {error}. Original sidecar was preserved."));
        }
        Ok(RecordingRecoveryResult {
            source_path: source.to_string_lossy().into_owned(),
            output_path: output.to_string_lossy().into_owned(),
            duration_ms: summary.manifest.segments.iter().map(|s| s.container_end_ns as f64 / 1_000_000.0).sum(),
            recovered_packets: summary.recovered_tails.iter().map(|s| s.recovered_packets).sum(),
            unparsed_tail_bytes: summary.recovered_tails.iter().map(|s| s.dropped_incomplete_bytes).sum(),
            final_trim_known: summary.recovered_tails.is_empty() && summary.rejected_segments.is_empty(),
            tail_reason: Some("Session segments remain separate playable files with their original track metadata.".into()),
            notes,
        })
    }
    /// Session recovery verifies finalized segments and repairs each tail separately.
    pub fn run_session(
        self,
        source: &Path,
        output: &Path,
    ) -> Result<frame_mux::SessionRecoverySummary, RecorderError> {
        frame_mux::recover_session(source, output)
    }
    pub fn run(
        self,
        source: &Path,
        output: &Path,
    ) -> Result<RecordingRecoveryResult, RecorderError> {
        if !source.is_absolute() || !output.is_absolute() {
            return Err(refusal(
                ErrorCode::InvalidRequest,
                "Choose absolute source and destination file paths.",
            ));
        }
        // recover opens the source read-only, creates the output exclusively and
        // removes its own incomplete output on failure. No capture is started.
        let summary = frame_mux::recover(source, output)?;
        Ok(RecordingRecoveryResult {
            source_path: summary.source,
            output_path: summary.output,
            duration_ms: summary.duration_ns as f64 / 1_000_000.0,
            recovered_packets: summary.recovered_packets,
            unparsed_tail_bytes: summary.dropped_incomplete_bytes,
            final_trim_known: summary.final_trim_known,
            tail_reason: summary.unknown_tail.reason,
            notes: summary.notes,
        })
    }
}

impl Drop for RecoveryJob {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn refusal(code: ErrorCode, message: &str) -> RecorderError {
    RecorderError {
        code,
        message: message.into(),
        action: "Choose an existing FREYM H.264/HEVC/Opus MKV and a new output file.".into(),
        recovery: RecoveryStatus::NoRecording,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recovery_admission_is_single_job_and_released_after_failure() {
        let jobs = RecoveryJobs::default();
        assert!(jobs.begin(&RecorderState::Recording).is_err());
        let job = jobs.begin(&RecorderState::Idle).unwrap();
        assert!(jobs.begin(&RecorderState::Idle).is_err());
        assert_eq!(
            job.run(Path::new("relative.mkv"), Path::new("output.mkv"))
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        assert!(jobs.begin(&RecorderState::Idle).is_ok());
    }
}
