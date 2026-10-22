//! Error shaping shared by the Matroska writers.
//!
//! Every rejection names the request or the condition it came from, and a container
//! failure is always reported as a failure of the container rather than of the
//! caller's data.

use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus};
use std::io;
use std::path::Path;

pub(super) const REQUEST_ACTION: &str = "Send one H.264 Annex-B video track whose timestamps increase strictly and whose DTS equals its PTS.";
pub(super) const PATH_ACTION: &str =
    "Choose a new output file path inside an existing writable directory.";
pub(super) const STORAGE_ACTION: &str =
    "Free disk space or choose another output path, then record again.";
pub(super) const BACKEND_ACTION: &str =
    "The container was not finalized; report this writer failure instead of using the file.";

pub(super) fn writer_error(
    code: ErrorCode,
    recovery: RecoveryStatus,
    message: impl Into<String>,
    action: &str,
) -> RecorderError {
    RecorderError {
        code,
        message: message.into(),
        recovery,
        action: action.to_string(),
    }
}

pub(super) fn request_error(message: impl Into<String>, recovery: RecoveryStatus) -> RecorderError {
    writer_error(ErrorCode::InvalidRequest, recovery, message, REQUEST_ACTION)
}

pub(super) fn backend_limit_error(message: impl Into<String>) -> RecorderError {
    writer_error(
        ErrorCode::Internal,
        RecoveryStatus::Unknown,
        message,
        BACKEND_ACTION,
    )
}

#[cfg(windows)]
const DISK_FULL_CODES: &[i32] = &[39, 112];
#[cfg(unix)]
const DISK_FULL_CODES: &[i32] = &[28];

fn is_disk_full(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::StorageFull
        || error
            .raw_os_error()
            .is_some_and(|code| DISK_FULL_CODES.contains(&code))
}

pub(super) fn output_path_error(path: &Path, error: &io::Error) -> RecorderError {
    writer_error(
        ErrorCode::InvalidOutputPath,
        RecoveryStatus::NoRecording,
        format!(
            "The output file {} could not be created: {error}",
            path.display()
        ),
        PATH_ACTION,
    )
}

pub(super) fn storage_error(error: &io::Error, what: &str) -> RecorderError {
    let code = if is_disk_full(error) {
        ErrorCode::DiskFull
    } else {
        ErrorCode::Internal
    };
    writer_error(
        code,
        RecoveryStatus::Unknown,
        format!("{what} {error}"),
        STORAGE_ACTION,
    )
}

pub(super) fn backend_error(error: mkwriter::Error, what: &str) -> RecorderError {
    match error {
        mkwriter::Error::Io(io_error) => storage_error(&io_error, what),
        other => writer_error(
            ErrorCode::Internal,
            RecoveryStatus::Unknown,
            format!("{what} {other}"),
            BACKEND_ACTION,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn storage_full_remains_disk_full_with_no_success_or_recovery_claim() {
        for error in [
            io::Error::from(io::ErrorKind::StorageFull),
            io::Error::from_raw_os_error(DISK_FULL_CODES[0]),
        ] {
            let shaped = storage_error(&error, "Recording write failed.");
            assert_eq!(shaped.code, ErrorCode::DiskFull);
            assert_eq!(shaped.recovery, RecoveryStatus::Unknown);
            assert_eq!(shaped.action, STORAGE_ACTION);
        }
        let other = storage_error(
            &io::Error::from(io::ErrorKind::PermissionDenied),
            "Recording write failed.",
        );
        assert_eq!(other.code, ErrorCode::Internal);
        assert_eq!(other.recovery, RecoveryStatus::Unknown);
    }
}
