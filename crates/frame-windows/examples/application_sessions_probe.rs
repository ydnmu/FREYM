// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Read-only Windows application audio session probe (FRAME-0106A).
//!
//! ```text
//! cargo run --locked -p frame-windows --example application_sessions_probe
//! cargo run --locked -p frame-windows --example application_sessions_probe -- --json
//! ```
//!
//! Options:
//!
//! - `--diagnostics` (the default) prints the human-readable report.
//! - `--json` prints one structured JSON document (pretty-printed) built from the same
//!   snapshot values with the workspace's existing `serde`/`serde_json` tooling.
//! - `--help` prints the usage.
//!
//! Both forms come from **one** observation, so the canonical sources and the evidence
//! beside them belong to the same pass.
//!
//! What it does: enumerates the application audio sessions Windows currently reports on
//! the active render endpoints and prints them, including why each record was skipped or
//! left unattributed and the resource bounds the pass enforced. What it does **not** do:
//! it captures no audio, opens no stream, toggles no system setting, starts no program and
//! writes no file. It is a diagnostic snapshot, not a recorder and not the deferred
//! FRAME-0106B process-isolated capture.
//!
//! An empty session list is a valid result and is reported as such; it is not proof that
//! discovery works, and a populated list is not proof that per-application isolation is
//! possible.

#[cfg(target_os = "windows")]
fn main() -> std::process::ExitCode {
    windows_probe_main()
}

#[cfg(not(target_os = "windows"))]
fn main() -> std::process::ExitCode {
    eprintln!("Application audio session discovery is implemented for Windows only.");
    std::process::ExitCode::FAILURE
}

#[cfg(target_os = "windows")]
fn windows_probe_main() -> std::process::ExitCode {
    use frame_windows::{application_audio_diagnostics, application_audio_session_snapshot};

    const USAGE: &str = "usage: application_sessions_probe [--json | --diagnostics] [--help]";

    let mut json = false;
    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "--json" => json = true,
            "--diagnostics" => json = false,
            "--help" | "-h" => {
                println!("{USAGE}");
                println!();
                println!("Read-only diagnostic snapshot of Windows application audio sessions.");
                println!("It captures no audio, changes no setting and starts no program.");
                return std::process::ExitCode::SUCCESS;
            }
            other => {
                eprintln!("{USAGE}");
                eprintln!("unknown argument: {other}");
                return std::process::ExitCode::FAILURE;
            }
        }
    }

    if !json {
        match application_audio_diagnostics() {
            Ok(report) => {
                print!("{report}");
                return std::process::ExitCode::SUCCESS;
            }
            Err(error) => {
                eprintln!(
                    "Application audio session diagnostics failed: {}",
                    error.message
                );
                return std::process::ExitCode::FAILURE;
            }
        }
    }

    let snapshot = match application_audio_session_snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            eprintln!(
                "Application audio session discovery failed: {}",
                error.message
            );
            eprintln!("code: {:?}  recovery: {:?}", error.code, error.recovery);
            eprintln!("action: {}", error.action);
            return std::process::ExitCode::FAILURE;
        }
    };

    match serde_json::to_string_pretty(&JsonReport::from(&snapshot)) {
        Ok(document) => {
            println!("{document}");
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("The application-session snapshot could not be serialized: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "windows")]
mod json_report {
    use serde::Serialize;

    use frame_contracts::AudioSource;
    use frame_windows::{
        ApplicationEndpointIssue, ApplicationEndpointRecord, ApplicationSessionCounters,
        ApplicationSessionReadFailure, ApplicationSessionRecord, ApplicationSessionSkipReason,
        ApplicationSessionSnapshot,
    };

    /// The enforced snapshot bounds, so the document reports the bounds the code applied
    /// instead of restating them.
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Limits {
        max_endpoints_per_snapshot: usize,
        max_sessions_per_snapshot: usize,
        max_native_string_utf16: usize,
        max_snapshot_payload_bytes: usize,
    }

    /// One endpoint and its session records, as raw evidence.
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Endpoint<'a> {
        endpoint_id: &'a str,
        friendly_name: Option<&'a str>,
        device_state: u32,
        issue: Option<Issue>,
        sessions: Vec<Session<'a>>,
    }

    /// One skipped or failed endpoint, with its category and detail.
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Issue {
        category: &'static str,
        detail: String,
        /// The API or native code, when the platform reported one.
        code: Option<String>,
    }

    /// One session record, including why it was or was not retained.
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Session<'a> {
        session_identifier: &'a str,
        instance_identifier: &'a str,
        /// The canonical identity, which is the same value `sources` carries.
        source_id: Option<&'a str>,
        raw_state: i32,
        is_active: bool,
        display_name: Option<&'a str>,
        is_system_sounds: bool,
        process_id: Option<u32>,
        process_availability: &'static str,
        executable_path: Option<&'a str>,
        executable_availability: &'static str,
        skip: Option<Skip>,
        read_failure: Option<ReadFailure<'a>>,
    }

    /// Why one session is not part of the returned sources.
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Skip {
        category: &'static str,
        detail: String,
    }

    /// One failed read inside a session record.
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct ReadFailure<'a> {
        operation: &'a str,
        detail: &'a str,
        hresult: Option<String>,
    }

    /// Every counter the snapshot reports, mirrored here so the JSON schema is declared
    /// in this read-only example: the library carries no serde dependency for a
    /// diagnostic, and no counter is omitted or renamed silently.
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Counters {
        sessions_total: usize,
        sessions_active: usize,
        sessions_inactive: usize,
        sessions_expired: usize,
        sessions_unknown_state: usize,
        sessions_system_sounds: usize,
        sessions_multiprocess: usize,
        sessions_single_process: usize,
        sessions_process_id_zero: usize,
        sessions_process_unavailable: usize,
        attributed_sources: usize,
        unattributed_sources: usize,
        sources_labelled_from_executable: usize,
        sources_labelled_unnamed: usize,
        session_reads_failed: usize,
        sessions_missing_identity: usize,
        metadata_reads_failed: usize,
        process_creation_times_observed: usize,
        endpoints_reported: usize,
        endpoints_retained: usize,
        endpoints_inactive: usize,
        payload_bytes: usize,
    }

    impl From<&ApplicationSessionCounters> for Counters {
        fn from(counters: &ApplicationSessionCounters) -> Self {
            Self {
                sessions_total: counters.sessions_total,
                sessions_active: counters.sessions_active,
                sessions_inactive: counters.sessions_inactive,
                sessions_expired: counters.sessions_expired,
                sessions_unknown_state: counters.sessions_unknown_state,
                sessions_system_sounds: counters.sessions_system_sounds,
                sessions_multiprocess: counters.sessions_multiprocess,
                sessions_single_process: counters.sessions_single_process,
                sessions_process_id_zero: counters.sessions_process_id_zero,
                sessions_process_unavailable: counters.sessions_process_unavailable,
                attributed_sources: counters.attributed_sources,
                unattributed_sources: counters.unattributed_sources,
                sources_labelled_from_executable: counters.sources_labelled_from_executable,
                sources_labelled_unnamed: counters.sources_labelled_unnamed,
                session_reads_failed: counters.session_reads_failed,
                sessions_missing_identity: counters.sessions_missing_identity,
                metadata_reads_failed: counters.metadata_reads_failed,
                process_creation_times_observed: counters.process_creation_times_observed,
                endpoints_reported: counters.endpoints_reported,
                endpoints_retained: counters.endpoints_retained,
                endpoints_inactive: counters.endpoints_inactive,
                payload_bytes: counters.payload_bytes,
            }
        }
    }

    /// The whole diagnostic document.
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    pub(super) struct JsonReport<'a> {
        task: &'static str,
        api: &'static str,
        /// What this document is not, stated in the machine-readable output itself.
        captures_audio: bool,
        implements_per_application_capture: bool,
        is_atomic_snapshot: bool,
        empty_result_is_valid: bool,
        identity_rule: &'static str,
        activity_rule: &'static str,
        source_count: usize,
        sources: &'a [AudioSource],
        counters: Counters,
        limits: Limits,
        endpoints: Vec<Endpoint<'a>>,
    }

    impl<'a> From<&'a ApplicationSessionSnapshot> for JsonReport<'a> {
        fn from(snapshot: &'a ApplicationSessionSnapshot) -> Self {
            let limits = snapshot.limits();
            Self {
                task: "FRAME-0106A",
                api: "MMDevice + IAudioSessionManager2/IAudioSessionEnumerator",
                captures_audio: false,
                implements_per_application_capture: false,
                is_atomic_snapshot: false,
                empty_result_is_valid: true,
                identity_rule: "wasapi-session:<endpoint id byte length>:<endpoint id utf-8 hex>:<session instance id byte length>:<session instance id utf-8 hex>:",
                activity_rule: "is_active is AudioSessionStateActive: the OS session state. It is not microphone permission, not proof that audio is flowing and not process-loopback availability.",
                source_count: snapshot.sources().len(),
                sources: snapshot.sources(),
                counters: Counters::from(&snapshot.counters),
                limits: Limits {
                    max_endpoints_per_snapshot: limits.max_endpoints_per_snapshot,
                    max_sessions_per_snapshot: limits.max_sessions_per_snapshot,
                    max_native_string_utf16: limits.max_native_string_utf16,
                    max_snapshot_payload_bytes: limits.max_snapshot_payload_bytes,
                },
                endpoints: snapshot.endpoints().iter().map(endpoint).collect(),
            }
        }
    }

    /// Renders one endpoint issue.
    fn issue(issue: &ApplicationEndpointIssue) -> Issue {
        match issue {
            ApplicationEndpointIssue::InactiveDeviceState { state, summary } => Issue {
                category: issue.category(),
                detail: (*summary).to_string(),
                code: Some(format!("DEVICE_STATE {state:#X}")),
            },
            ApplicationEndpointIssue::EndpointReadFailed {
                operation,
                detail,
                hresult,
                ..
            } => Issue {
                category: issue.category(),
                detail: detail.clone(),
                code: hresult
                    .map(hresult_text)
                    .or_else(|| Some(operation.clone())),
            },
        }
    }

    /// Renders one endpoint record.
    fn endpoint(endpoint: &ApplicationEndpointRecord) -> Endpoint<'_> {
        Endpoint {
            endpoint_id: &endpoint.endpoint_id,
            friendly_name: endpoint.friendly_name.as_deref(),
            device_state: endpoint.device_state,
            issue: endpoint.issue.as_ref().map(issue),
            sessions: endpoint.sessions.iter().map(session).collect(),
        }
    }

    /// Renders one session record.
    fn session(session: &ApplicationSessionRecord) -> Session<'_> {
        Session {
            session_identifier: &session.session_identifier,
            instance_identifier: &session.instance_identifier,
            source_id: session.source_id.as_ref().map(|id| id.0.as_str()),
            raw_state: session.raw_state,
            is_active: session.is_active,
            display_name: session.display_name.as_deref(),
            is_system_sounds: session.is_system_sounds,
            process_id: session.process_id,
            process_availability: session.process_availability.category(),
            executable_path: session.executable_path.as_deref(),
            executable_availability: session.executable_availability.category(),
            skip: session.skip_reason.as_ref().map(skip),
            read_failure: session.read_failure.as_ref().map(read_failure),
        }
    }

    /// Renders one skip reason as a category plus a sentence, so a consumer can filter on
    /// the category and read the detail.
    fn skip(reason: &ApplicationSessionSkipReason) -> Skip {
        let detail = match reason {
            ApplicationSessionSkipReason::SystemSoundsSession => {
                "IsSystemSoundsSession returned S_OK, so this is a system sounds session, not an application source".to_string()
            }
            ApplicationSessionSkipReason::ExpiredSession => {
                "the session state is AudioSessionStateExpired, so the session is gone".to_string()
            }
            ApplicationSessionSkipReason::SessionReadFailed {
                operation,
                detail,
                hresult,
            } => format!(
                "reading {operation} failed ({detail}{}), so this record is partial and unusable; unrelated sessions are unaffected",
                hresult
                    .map(hresult_text)
                    .map(|code| format!(", {code}"))
                    .unwrap_or_default()
            ),
            ApplicationSessionSkipReason::MissingSessionIdentity { missing } => format!(
                "Windows reported an empty {missing}, and no identity is invented for a session"
            ),
            ApplicationSessionSkipReason::UnknownSessionState { raw_state } => format!(
                "the session reported state {raw_state}, which is none of the three documented AudioSessionState values, so no is_active value is guessed"
            ),
        };
        Skip {
            category: reason.category(),
            detail,
        }
    }

    /// Renders one session read failure.
    fn read_failure(failure: &ApplicationSessionReadFailure) -> ReadFailure<'_> {
        ReadFailure {
            operation: &failure.operation,
            detail: &failure.detail,
            hresult: failure.hresult.map(hresult_text),
        }
    }

    /// Renders an HRESULT the way the human-readable report does.
    fn hresult_text(code: i32) -> String {
        format!("0x{:08X}", code as u32)
    }
}

#[cfg(target_os = "windows")]
use json_report::JsonReport;
