//! Recording-specific output rules; no shared settings schema or media-path changes.
use chrono::{DateTime, Local};
use frame_contracts::{EngineWarning, ErrorCode, RecorderError, RecoveryStatus};
use std::{
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, SyncSender, sync_channel},
    time::{Duration, Instant},
};

pub const DEFAULT_FILENAME_TEMPLATE: &str = "FREYM_{date}_{time}";
pub const LOW_SPACE_WARNING_BYTES: u64 = 1024 * 1024 * 1024;
const PROBE_INTERVAL: Duration = Duration::from_secs(5);

fn invalid(message: impl Into<String>) -> RecorderError {
    RecorderError {
        code: ErrorCode::InvalidOutputPath,
        message: message.into(),
        recovery: RecoveryStatus::NoRecording,
        action: "Choose an existing absolute recording directory and a valid filename or template."
            .into(),
    }
}

pub fn validate_filename_template(template: &str) -> Result<(), RecorderError> {
    expand_filename(template, Local::now()).map(|_| ())
}

/// Template tokens expand once at Start. A plain stem remains an exact filename.
/// Unknown/unbalanced braces are errors, rather than silently literal tokens.
pub fn expand_filename(template: &str, now: DateTime<Local>) -> Result<String, RecorderError> {
    let mut rest = template.trim();
    let mut result = String::new();
    while let Some(open) = rest.find('{') {
        let literal = &rest[..open];
        if literal.contains('}') {
            return Err(invalid("Unbalanced filename template braces."));
        }
        result.push_str(literal);
        rest = &rest[open + 1..];
        let Some(close) = rest.find('}') else {
            return Err(invalid("Unbalanced filename template braces."));
        };
        let pattern = match &rest[..close] {
            "date" => "%Y-%m-%d",
            "time" => "%H-%M-%S",
            "timestamp" => "%Y-%m-%d_%H-%M-%S",
            _ => {
                return Err(invalid(
                    "Supported filename tokens are {date}, {time}, and {timestamp}.",
                ));
            }
        };
        result.push_str(&now.format(pattern).to_string());
        rest = &rest[close + 1..];
    }
    if rest.contains('}') {
        return Err(invalid("Unbalanced filename template braces."));
    }
    result.push_str(rest);
    validate_filename(&result)?;
    Ok(result)
}

fn validate_filename(stem: &str) -> Result<(), RecorderError> {
    let device = stem
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches([' ', '.'])
        .to_ascii_uppercase();
    let reserved = matches!(
        device.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        device.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    });
    // Bound a filename component, including .mkv and collision suffixes, for
    // common Windows and Unix filesystems. Non-ASCII filenames remain supported.
    if stem.is_empty()
        || stem == "."
        || stem == ".."
        || reserved
        || stem.ends_with([' ', '.'])
        || stem.len() > 200
        || stem.encode_utf16().count() > 200
        || stem
            .chars()
            .any(|c| c.is_control() || "/\\:*?\"<>|".contains(c))
    {
        return Err(invalid(
            "Use a plain filename without paths, device names, trailing dots/spaces, or reserved characters (maximum 200 bytes).",
        ));
    }
    Ok(())
}

/// Checks without creating or truncating anything. Only automatic/template names
/// acquire suffixes; exact caller-supplied stems still refuse collisions. The
/// writer's exclusive create remains authoritative when two starts race.
pub fn resolve_output(
    directory: &Path,
    stem: &str,
    now: DateTime<Local>,
) -> Result<PathBuf, RecorderError> {
    if !directory.is_absolute() || !directory.is_dir() {
        return Err(invalid(format!(
            "Recording directory {} must be an existing absolute directory.",
            directory.display()
        )));
    }
    let automatic = stem == crate::recording_control::AUTO_FILE_STEM;
    let template = if automatic {
        DEFAULT_FILENAME_TEMPLATE
    } else {
        stem
    };
    let expanded = expand_filename(template, now)?;
    let numbered = automatic || template.contains('{');
    select_output(directory, &expanded, numbered)
}

pub(crate) fn select_output(
    directory: &Path,
    stem: &str,
    numbered: bool,
) -> Result<PathBuf, RecorderError> {
    validate_filename(stem)?;
    for number in 1..=if numbered { 10_000 } else { 1 } {
        let name = if number == 1 {
            format!("{stem}.mkv")
        } else {
            format!("{stem}_{number}.mkv")
        };
        let candidate = directory.join(name);
        // A session/journal/sidecar owns this basename too, even if its MKV has
        // disappeared. Do not mix a new recording with an interrupted session.
        let mut occupied = false;
        for path in [
            &candidate,
            &candidate.with_extension("mkv.freym-session"),
            &candidate.with_extension("mkv.freym-incomplete"),
            &candidate.with_extension("mkv.freym-session.freym-incomplete"),
            &candidate.with_extension("freym-replay"),
        ] {
            match std::fs::symlink_metadata(path) {
                Ok(_) => {
                    occupied = true;
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(invalid(format!("Cannot check {}: {e}", path.display()))),
            }
        }
        if !occupied {
            return Ok(candidate);
        }
    }
    Err(invalid(
        "The recording name is already owned by a file/session, or no unused template name remains.",
    ))
}

pub fn storage_warning(result: Result<u64, String>) -> Option<EngineWarning> {
    match result {
        Ok(bytes) if bytes < LOW_SPACE_WARNING_BYTES => Some(EngineWarning {
            code: "recordingDiskSpaceLow".into(),
            message: format!("The recording volume has {bytes} bytes available to this user (warning below {LOW_SPACE_WARNING_BYTES} bytes)."),
            action: "Free space or stop recording soon. A write failure stops capture and preserves incomplete files for recovery.".into(),
        }),
        Ok(_) => None,
        Err(error) => Some(EngineWarning { code: "recordingDiskSpaceUnknown".into(),
            message: format!("Recording disk space could not be measured: {error}"),
            action: "Check the recording volume. Recording can continue until the writer reports a storage failure.".into() }),
    }
}

struct ProbeRequest {
    session: String,
    directory: PathBuf,
}
struct ProbeResult {
    session: String,
    directory: PathBuf,
    result: Result<u64, String>,
}

/// One optional, bounded, read-only worker per control owner. A slow disk query
/// never stalls capture, encoding, Stop or teardown. No media resource is handed
/// to this worker. On Drop a blocked OS query may finish later; it cannot publish
/// events after its receiver goes away and receives no further work.
pub(crate) struct StorageMonitor {
    requests: SyncSender<ProbeRequest>,
    results: Receiver<ProbeResult>,
    last_request: Option<Instant>,
    last_poll: Option<Instant>,
    session: Option<String>,
    low_sent: bool,
    unknown_sent: bool,
}

impl StorageMonitor {
    pub fn new() -> Option<Self> {
        let (requests, incoming) = sync_channel::<ProbeRequest>(1);
        let (completed, results) = sync_channel(1);
        std::thread::Builder::new()
            .name("freym-recording-space".into())
            .spawn(move || {
                while let Ok(request) = incoming.recv() {
                    #[cfg(target_os = "windows")]
                    let result =
                        frame_windows::recording_storage::available_bytes(&request.directory)
                            .map_err(|e| e.to_string());
                    #[cfg(not(target_os = "windows"))]
                    let result =
                        Err("This platform has no connected recording-volume probe.".into());
                    let observation = ProbeResult {
                        session: request.session,
                        directory: request.directory,
                        result,
                    };
                    if completed.send(observation).is_err() {
                        break;
                    }
                }
            })
            .ok()?;
        Some(Self {
            requests,
            results,
            last_request: None,
            last_poll: None,
            session: None,
            low_sent: false,
            unknown_sent: false,
        })
    }

    pub fn due(&mut self, now: Instant) -> bool {
        if self
            .last_poll
            .is_some_and(|last| now.saturating_duration_since(last) < Duration::from_millis(250))
        {
            return false;
        }
        self.last_poll = Some(now);
        true
    }

    pub fn poll(&mut self, session: &str, directory: &Path, now: Instant) -> Option<EngineWarning> {
        if self.session.as_deref() != Some(session) {
            self.session = Some(session.into());
            self.last_request = None;
            self.low_sent = false;
            self.unknown_sent = false;
        }
        let mut warning = None;
        while let Ok(observation) = self.results.try_recv() {
            if observation.session != session || observation.directory != directory {
                continue;
            }
            if let Some(next) = storage_warning(observation.result) {
                let sent = if next.code == "recordingDiskSpaceLow" {
                    &mut self.low_sent
                } else {
                    &mut self.unknown_sent
                };
                if !*sent {
                    *sent = true;
                    warning = Some(next);
                }
            }
        }
        if self
            .last_request
            .is_none_or(|last| now.saturating_duration_since(last) >= PROBE_INTERVAL)
            && self
                .requests
                .try_send(ProbeRequest {
                    session: session.into(),
                    directory: directory.into(),
                })
                .is_ok()
        {
            self.last_request = Some(now);
        }
        warning
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn monitor_is_bounded_ignores_stale_results_and_warns_once_per_session() {
        let (requests, incoming) = sync_channel(1);
        let (completed, results) = sync_channel(1);
        let mut monitor = StorageMonitor {
            requests,
            results,
            last_request: None,
            last_poll: None,
            session: None,
            low_sent: false,
            unknown_sent: false,
        };
        let directory = Path::new("C:/recording-volume");
        let now = Instant::now();
        assert!(monitor.due(now));
        assert!(!monitor.due(now + Duration::from_millis(100)));
        assert!(monitor.poll("first", directory, now).is_none());
        // Leave the request pending: later polls must return without blocking.
        assert!(
            monitor
                .poll("first", directory, now + PROBE_INTERVAL)
                .is_none()
        );
        let request = incoming.try_recv().unwrap();
        assert_eq!(request.session, "first");
        completed
            .send(ProbeResult {
                session: request.session,
                directory: request.directory,
                result: Ok(0),
            })
            .unwrap();
        assert!(
            monitor
                .poll("second", directory, now + PROBE_INTERVAL)
                .is_none(),
            "old-session disk readings must not warn a new session"
        );
        let request = incoming.try_recv().unwrap();
        completed
            .send(ProbeResult {
                session: request.session,
                directory: request.directory,
                result: Ok(0),
            })
            .unwrap();
        assert_eq!(
            monitor
                .poll("second", directory, now + PROBE_INTERVAL)
                .unwrap()
                .code,
            "recordingDiskSpaceLow"
        );
        completed
            .send(ProbeResult {
                session: "second".into(),
                directory: directory.into(),
                result: Ok(0),
            })
            .unwrap();
        assert!(
            monitor
                .poll("second", directory, now + PROBE_INTERVAL)
                .is_none()
        );
        completed
            .send(ProbeResult {
                session: "second".into(),
                directory: directory.into(),
                result: Err("disconnected".into()),
            })
            .unwrap();
        assert_eq!(
            monitor
                .poll("second", directory, now + PROBE_INTERVAL)
                .unwrap()
                .code,
            "recordingDiskSpaceUnknown"
        );
    }
}
