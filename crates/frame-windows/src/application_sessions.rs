// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Windows application audio session discovery (FRAME-0106A, ADR 0007).
//!
//! This module answers one question, read-only: **which audio sessions does Windows
//! currently know about on the active render endpoints, and what can be truthfully
//! said about each one?** It uses MMDevice and
//! `IAudioSessionManager2`/`IAudioSessionEnumerator` on a fresh joined MTA worker and
//! returns canonical [`AudioSource`] values.
//!
//! # What this is, and what it is not
//!
//! - **It is a diagnostic, non-atomic snapshot.** Every call enumerates again from
//!   scratch. The individual OS reads inside one pass are not atomic, so a session
//!   created, destroyed or moved between two reads can be missing, or can be described
//!   by values that belonged to an earlier moment. A snapshot is not a live tracker:
//!   there is no notification service, no global registry and no change subscription
//!   here, and none is claimed.
//! - **It captures nothing.** No `IAudioClient`, no activation, no loopback, no PCM,
//!   no volume or privacy change, no arbitrary process start. It reads session
//!   metadata only.
//! - **It does not implement per-application capture.** Enumerating a session is not
//!   the same as capturing its audio, and it is not evidence that a later isolated
//!   capture is possible. That boundary belongs to the deferred FRAME-0106B, whose
//!   process-loopback activation and lifecycle have their own documented OS boundary.
//! - **It enables no capability.** `EngineCapabilities::per_app_audio` stays false;
//!   `AudioBackend` composes these sources with microphone endpoints, without adding
//!   an IPC command or changing the wire schema.
//! - **An empty result is valid.** A successful snapshot with no sessions is a real
//!   result, not proof that discovery works and not proof that sessions were missed.
//!
//! # Identity
//!
//! A source ID is a **live-session selection identity**, not a permanent route rule for
//! an application. It is a session instance plus the endpoint it lives on, so two
//! sessions of the same executable - or the same application restarted - have
//! different IDs, and the same session keeps its ID while it lives. Identity is never
//! derived from a display name, an executable path, a PID or an enumeration index, so
//! several sessions that share a name or a path stay distinct, and one session cannot
//! collide with another. See [`application_session_id`] for the exact encoding.
//!
//! `app_identifier` is a *best-effort path-based route identity*: `win-exe:` plus the
//! full image path Windows reported for the session's process. It is not file
//! identity, not a security guarantee, and `None` when the path could not be read.
//! PID and process creation time are lifetime evidence for a later capture start-time
//! revalidation; a PID alone is never claimed to survive a restart or reuse.
//!
//! # Bounds and failure
//!
//! Bounds live in [`limits`]. Exceeding any of them fails the snapshot explicitly.
//! Recoverable outcomes stay in the snapshot where they are recoverable and honest:
//! a session that vanished mid-pass, a process whose image path is denied, a system
//! sounds session or an expired session are all recorded with a reason instead of
//! being dropped. The two failures that can never become a fabricated empty success
//! are endpoint enumeration and a session enumeration that reports a count it does not
//! deliver.

mod limits;
#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
mod native;
// The synthetic tests drive the production mapping rules and the joined-worker boundary
// through the private observation seam. They are compiled only for a Windows test build
// because several states they construct - a denied process read, an unavailable image
// path, a bound violation - are produced only by the Windows reader; compiling them
// elsewhere would require dead copies of those states in this Windows-only backend.
#[cfg(all(test, target_os = "windows"))]
mod tests;
mod worker;

/// The one accepted set of snapshot limits: the published bounds the ledger, the
/// collector and the mapper all enforce.
pub use limits::ApplicationSessionLimits;

/// The one accepted set of snapshot limits: the published bounds the ledger, the collector
/// and the mapper all enforce.
pub const APPLICATION_SESSION_LIMITS: ApplicationSessionLimits = limits::APPLICATION_SESSION_LIMITS;

use limits::SnapshotBudget;
use worker::{ApplicationObservationSource, ObservationHeader, ObservedSession, WideString};

use frame_contracts::{AudioRoute, AudioSource, AudioSourceId, AudioSourceKind, RecorderError};

/// The literal label used when Windows supplies no usable session name and no readable
/// executable path exists either. It is an honest label, not an application identity.
pub(crate) const UNNAMED_SESSION_LABEL: &str = "unnamed session";

/// The `app_identifier` prefix for a best-effort executable path.
pub(crate) const APP_IDENTIFIER_PREFIX: &str = "win-exe:";

/// The source-ID prefix for an application audio session (ADR 0007).
pub(crate) const SESSION_ID_PREFIX: &str = "wasapi-session:";

// Fixed per-record overhead the byte ledger charges before a record is created. These are
// deliberately generous: the ledger is an upper bound on the snapshot's owned storage, so
// over-counting a `Vec` header or a borrowed `AudioSourceId` costs headroom and never
// costs correctness.
const RESERVED_ENDPOINT_RECORD_BYTES: usize = 512;
const RESERVED_SESSION_RECORD_BYTES: usize = 1024;
const RESERVED_SOURCE_RECORD_BYTES: usize = 256;
const RESERVED_METADATA_FAILURE_BYTES: usize = 256;

/// Why an enumerated session is not part of the returned sources.
///
/// This is diagnostic evidence, not an error: discovery of every other session
/// continues. A reason carries the native/API code where one exists.
#[derive(Debug, Clone, PartialEq)]
pub enum ApplicationSessionSkipReason {
    /// `IsSystemSoundsSession` returned `S_OK`: the OS owns this session, so it is not
    /// an application source. Excluded from the returned sources.
    SystemSoundsSession,
    /// `AudioSessionStateExpired`: the session is gone.
    ExpiredSession,
    /// The session record could not be read; the API/HRESULT code and detail are
    /// preserved.
    SessionReadFailed {
        operation: String,
        detail: String,
        hresult: Option<i32>,
    },
    /// The session identifier or session instance identifier was empty. Neither is
    /// invented, so the session cannot be given an honest identity.
    MissingSessionIdentity { missing: &'static str },
    /// The session did not report one of the three documented states, so no `is_active`
    /// value is guessed for it.
    UnknownSessionState { raw_state: i32 },
}

impl ApplicationSessionSkipReason {
    /// A short stable category for logs and JSON.
    pub fn category(&self) -> &'static str {
        match self {
            Self::SystemSoundsSession => "systemSoundsSession",
            Self::ExpiredSession => "expiredSession",
            Self::SessionReadFailed { .. } => "sessionReadFailed",
            Self::MissingSessionIdentity { .. } => "missingSessionIdentity",
            Self::UnknownSessionState { .. } => "unknownSessionState",
        }
    }
}

/// Why an enumerated active render endpoint contributed no sessions.
#[derive(Debug, Clone, PartialEq)]
pub enum ApplicationEndpointIssue {
    /// MMDevice reported an active render endpoint whose state no longer reads as
    /// active, so its sessions were not treated as application sources.
    InactiveDeviceState { state: u32, summary: &'static str },
    /// A read that stopped this endpoint's part of the pass. Recorded for the diagnostic
    /// report; the snapshot itself fails, so this variant never appears in a returned
    /// snapshot.
    EndpointReadFailed {
        label: String,
        operation: String,
        detail: String,
        hresult: Option<i32>,
    },
}

impl ApplicationEndpointIssue {
    /// A short stable category for logs and JSON.
    pub fn category(&self) -> &'static str {
        match self {
            Self::InactiveDeviceState { .. } => "inactiveDeviceState",
            Self::EndpointReadFailed { .. } => "endpointReadFailed",
        }
    }
}

/// Which process identifier attribute a session has, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplicationProcessAvailability {
    /// `GetProcessId` returned `S_OK` with a single nonzero process ID.
    SingleProcess,
    /// `GetProcessId` returned the documented positive status
    /// `AUDCLNT_S_NO_SINGLE_PROCESS`: the session spans more than one process, so no
    /// single owner is reported.
    MultiProcessSession,
    /// The session has no readable process owner (a denied `OpenProcess`, a vanished
    /// process, or `GetProcessId` refusing).
    Unavailable,
    /// `GetProcessId` returned a single process identity of `0`, which is not a usable
    /// process identifier.
    SingleProcessZero,
}

impl ApplicationProcessAvailability {
    /// A short stable category for logs and JSON.
    pub fn category(&self) -> &'static str {
        match self {
            Self::SingleProcess => "singleProcess",
            Self::MultiProcessSession => "multiProcessSession",
            Self::Unavailable => "unavailable",
            Self::SingleProcessZero => "singleProcessZero",
        }
    }
}

/// What one `IAudioSessionControl2::GetProcessId` status means.
///
/// Both documented positive statuses are represented, which is the point: `S_OK` is one
/// process and `AUDCLNT_S_NO_SINGLE_PROCESS` is more than one, and the second must not be
/// collapsed into the first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApplicationProcessIdStatus {
    /// `S_OK` with a single nonzero process identifier.
    SingleProcess { process_id: u32 },
    /// `S_OK` with a zero process identifier, which is not a usable process.
    SingleProcessZero,
    /// `AUDCLNT_S_NO_SINGLE_PROCESS`: the session spans more than one process.
    NoSingleProcess,
    /// The call failed; the `HRESULT` is preserved.
    Failed { hresult: i32 },
}

/// What one `IAudioSessionControl2::IsSystemSoundsSession` status means.
///
/// `S_OK` and `S_FALSE` are both successes with opposite meanings. The official
/// documentation: "S_OK - The session is a system sounds session." and "S_FALSE - The
/// session is not a system sounds session."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApplicationSystemSoundsStatus {
    IsSystemSounds,
    IsNotSystemSounds,
    /// The call failed; the `HRESULT` is preserved.
    Failed {
        hresult: i32,
    },
}

/// Classifies a raw `GetProcessId` `HRESULT` and out-value pair.
///
/// This is the whole decision, in one place, so the two positive statuses cannot drift
/// apart between the native reader and the tests.
///
/// `AUDCLNT_S_NO_SINGLE_PROCESS` is the documented positive status
/// `AUDCLNT_SUCCESS(0x00d)`, i.e. `MAKE_SCODE(SEVERITY_SUCCESS, FACILITY_AUDCLNT, 0x00d)`
/// with `FACILITY_AUDCLNT == 2185`, which is `0x0889000D`. The `windows` crate does not
/// export that constant in any version in this lockfile, and the installed Windows SDK
/// headers do not define it either, so the documented value is stated here once.
pub(crate) fn classify_process_id_status(
    hresult: i32,
    has_out_value: bool,
    process_id: u32,
) -> ApplicationProcessIdStatus {
    const AUDCLNT_S_NO_SINGLE_PROCESS: i32 = 0x0889_000D_u32 as i32;
    if hresult == 0 {
        if !has_out_value {
            return ApplicationProcessIdStatus::Failed { hresult };
        }
        return if process_id == 0 {
            ApplicationProcessIdStatus::SingleProcessZero
        } else {
            ApplicationProcessIdStatus::SingleProcess { process_id }
        };
    }
    if hresult == AUDCLNT_S_NO_SINGLE_PROCESS {
        // The initial creator's identifier is written, but it is not a sole owner, so it
        // is deliberately discarded here.
        return ApplicationProcessIdStatus::NoSingleProcess;
    }
    ApplicationProcessIdStatus::Failed { hresult }
}

/// Classifies a raw `IsSystemSoundsSession` `HRESULT`.
pub(crate) fn classify_system_sounds_status(hresult: i32) -> ApplicationSystemSoundsStatus {
    /// `S_OK`.
    const S_OK: i32 = 0;
    /// `S_FALSE`.
    const S_FALSE: i32 = 1;
    match hresult {
        S_OK => ApplicationSystemSoundsStatus::IsSystemSounds,
        S_FALSE => ApplicationSystemSoundsStatus::IsNotSystemSounds,
        hresult => ApplicationSystemSoundsStatus::Failed { hresult },
    }
}

/// Which executable-path attribute a session has, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplicationExecutableAvailability {
    /// The full image path was read and is the basis of `app_identifier`.
    Available,
    /// The session has no single process, so no application path is attributed.
    NoSingleProcess,
    /// The process could not be opened: this process has no access to it, or it ended
    /// during the pass. Includes the native error code in the diagnostic detail.
    ProcessUnavailable,
    /// The process was opened but its image path query failed.
    ImagePathUnavailable,
}

impl ApplicationExecutableAvailability {
    /// A short stable category for logs and JSON.
    pub fn category(&self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::NoSingleProcess => "noSingleProcess",
            Self::ProcessUnavailable => "processUnavailable",
            Self::ImagePathUnavailable => "imagePathUnavailable",
        }
    }
}

/// One endpoint as observed, with its retained sessions.
#[derive(Debug, Clone, PartialEq)]
pub struct ApplicationEndpointRecord {
    /// Opaque endpoint ID from `IMMDevice::GetId` (the same value as FRAME-0105's
    /// microphone identity prefix uses, under this module's own prefix).
    pub endpoint_id: String,
    /// OS device friendly name, informational text only. Never identity.
    pub friendly_name: Option<String>,
    /// The device state read during this pass, so a device that stopped being active is
    /// recorded rather than silently empty.
    pub device_state: u32,
    /// The session records read on this endpoint, in the order Windows returned them.
    pub sessions: Vec<ApplicationSessionRecord>,
    /// Why this endpoint contributed nothing, when that is known.
    pub issue: Option<ApplicationEndpointIssue>,
}

/// One session record read from the OS, converted to owned values.
///
/// Every field here is Frame-owned; no COM interface, pointer or handle crosses the
/// worker boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct ApplicationSessionRecord {
    /// The OS session identifier, which several instances can share.
    pub session_identifier: String,
    /// The OS session *instance* identifier, unique across instances.
    pub instance_identifier: String,
    /// Canonical [`AudioSourceId`] value, or `None` when a component of the identity was
    /// empty and the session was skipped.
    pub source_id: Option<AudioSourceId>,
    /// The raw `AudioSessionState` value as reported.
    pub raw_state: i32,
    /// `AudioSessionStateActive` for this session at read time.
    pub is_active: bool,
    /// OS session display name, when the OS supplied a nonempty one.
    pub display_name: Option<String>,
    /// `IsSystemSoundsSession` returned `S_OK`.
    pub is_system_sounds: bool,
    /// The process identifier Windows reported, when it reported exactly one.
    pub process_id: Option<u32>,
    /// What `GetProcessId` actually said, including the positive multi-process status
    /// that the generated convenience wrapper erases.
    pub process_availability: ApplicationProcessAvailability,
    /// The full image path of that process, when it was read.
    pub executable_path: Option<String>,
    /// Why an executable path is or is not present.
    pub executable_availability: ApplicationExecutableAvailability,
    /// Optional process-metadata reads that failed. These never excluded the session; the
    /// source is retained with `app_identifier = None` and this reason.
    pub metadata_failures: Vec<ApplicationSessionReadFailure>,
    /// Whether reading this record's essential identity, state or process id failed.
    /// When set together with a skip reason, the session was excluded because the OS
    /// reported that it had ended.
    pub read_failure: Option<ApplicationSessionReadFailure>,
    /// The final reason this session is not in the returned sources, when it is not.
    pub skip_reason: Option<ApplicationSessionSkipReason>,
}

/// One failed read inside a session record.
#[derive(Debug, Clone, PartialEq)]
pub struct ApplicationSessionReadFailure {
    pub operation: String,
    pub detail: String,
    pub hresult: Option<i32>,
}

/// Per-snapshot accounting for every session and endpoint the pass saw.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplicationSessionCounters {
    /// Sessions read in this snapshot, including skipped ones.
    pub sessions_total: usize,
    /// `AudioSessionStateActive`.
    pub sessions_active: usize,
    /// `AudioSessionStateInactive`, retained with `is_active = false`.
    pub sessions_inactive: usize,
    /// `AudioSessionStateExpired`, excluded.
    pub sessions_expired: usize,
    /// Sessions whose state was not one of the three documented values, excluded.
    pub sessions_unknown_state: usize,
    /// System sounds sessions, excluded.
    pub sessions_system_sounds: usize,
    /// `AUDCLNT_S_NO_SINGLE_PROCESS`: more than one process owns the session, so no
    /// single `process_id` is reported.
    pub sessions_multiprocess: usize,
    /// Sessions with a single process owner.
    pub sessions_single_process: usize,
    /// Sessions with a single process identity of `0`, which is not usable.
    pub sessions_process_id_zero: usize,
    /// Sessions with no readable process owner.
    pub sessions_process_unavailable: usize,
    /// Sessions in the returned sources with a `win-exe:` `app_identifier`.
    pub attributed_sources: usize,
    /// Sessions in the returned sources without an `app_identifier`.
    pub unattributed_sources: usize,
    /// Sources whose label is the executable basename because the OS name was empty.
    pub sources_labelled_from_executable: usize,
    /// Sources whose label is the literal unnamed-session label.
    pub sources_labelled_unnamed: usize,
    /// Session reads that failed; each is diagnosed and excluded, and unrelated
    /// sessions are unaffected.
    pub session_reads_failed: usize,
    /// Sessions whose identity was empty and could not be formed.
    pub sessions_missing_identity: usize,
    /// Optional process-metadata reads that failed while their session was retained.
    pub metadata_reads_failed: usize,
    /// Sessions retained with a private process creation time observed.
    pub process_creation_times_observed: usize,
    /// Endpoints Windows reported per enumeration pass.
    pub endpoints_reported: usize,
    /// Endpoints retained (the records).
    pub endpoints_retained: usize,
    /// Endpoints that were not active at read time and so contributed no sessions.
    pub endpoints_inactive: usize,
    /// Frame-owned bytes charged for this snapshot.
    pub payload_bytes: usize,
}

/// One complete, self-describing application-session snapshot.
///
/// The canonical values are [`ApplicationSessionSnapshot::sources`]; everything else is
/// the evidence and accounting that explains them, including why records were skipped.
#[derive(Debug, Clone, PartialEq)]
pub struct ApplicationSessionSnapshot {
    pub sources: Vec<AudioSource>,
    pub endpoints: Vec<ApplicationEndpointRecord>,
    pub counters: ApplicationSessionCounters,
    pub limits: ApplicationSessionLimits,
}

impl ApplicationSessionSnapshot {
    /// The canonical sources, deterministically ordered by identity.
    pub fn sources(&self) -> &[AudioSource] {
        &self.sources
    }

    /// The per-endpoint observation records behind those sources.
    pub fn endpoints(&self) -> &[ApplicationEndpointRecord] {
        &self.endpoints
    }

    /// The accounting for every session and endpoint this pass saw.
    pub fn counters(&self) -> ApplicationSessionCounters {
        self.counters
    }

    /// The bounds this snapshot was checked against.
    pub fn limits(&self) -> ApplicationSessionLimits {
        self.limits
    }

    /// Takes the canonical sources.
    pub fn into_sources(self) -> Vec<AudioSource> {
        self.sources
    }
}

/// The canonical source identity for one session on one endpoint.
///
/// Encoding (ADR 0007): `wasapi-session:` followed by the byte length of the endpoint ID
/// as decimal ASCII, `:`, the endpoint ID as lowercase hex UTF-8 bytes, `:`, the byte
/// length of the session instance ID, `:`, and that ID as lowercase hex UTF-8 bytes.
///
/// Both components are length-prefixed, so no endpoint or session value can ever be
/// re-parsed from a concatenation and no separator-like content is ambiguous. The hex
/// is a transport encoding of the component's UTF-8 bytes: it keeps the canonical
/// `AudioSourceId` string printable ASCII, which the wire contract, the generated
/// TypeScript and the frontend all treat as an opaque string, while the length prefixes
/// remain the authoritative framing. A session instance identifier is unique across
/// instances (Microsoft documents it as unique even for two instances of one
/// application), so this value is a live-session identity, not a durable route rule.
pub fn application_session_id(endpoint_id: &str, instance_identifier: &str) -> AudioSourceId {
    let mut value = String::with_capacity(session_id_capacity(endpoint_id, instance_identifier));
    value.push_str(SESSION_ID_PREFIX);
    append_component(&mut value, endpoint_id);
    append_component(&mut value, instance_identifier);
    AudioSourceId(value)
}

fn session_id_capacity(endpoint_id: &str, instance_identifier: &str) -> usize {
    let component = |text: &str| text.len().to_string().len() + 2 + text.len() * 2;
    SESSION_ID_PREFIX.len() + component(endpoint_id) + component(instance_identifier)
}

/// Appends one length-prefixed hex component to an identity under construction.
fn append_component(value: &mut String, component: &str) {
    use std::fmt::Write;
    let _ = write!(value, "{}", component.len());
    value.push(':');
    for byte in component.as_bytes() {
        value.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        value.push(char::from_digit(u32::from(byte & 0x0F), 16).unwrap_or('0'));
    }
    value.push(':');
}

/// Re-decodes an identity produced by [`application_session_id`].
///
/// Private, and used by the tests and the report renderer so the encoding round-trips
/// through one implementation instead of two. `None` means the value is not a
/// well-formed application-session identity.
pub(crate) fn decode_application_session_id(value: &str) -> Option<(String, String)> {
    let rest = value.strip_prefix(SESSION_ID_PREFIX)?;
    let (endpoint, rest) = take_component(rest)?;
    let (instance, trailing) = take_component(rest)?;
    if !trailing.is_empty() {
        return None;
    }
    Some((endpoint, instance))
}

/// Reads one `<byte-length>:<hex>:` component.
fn take_component(value: &str) -> Option<(String, &str)> {
    let (length, rest) = value.split_once(':')?;
    let length: usize = length.parse().ok()?;
    let hex = rest.get(..length.checked_mul(2)?)?;
    let trailing = rest.get(length.checked_mul(2)?..)?.strip_prefix(':')?;
    let mut bytes = Vec::with_capacity(length);
    let mut digits = hex.chars();
    while let (Some(high), Some(low)) = (digits.next(), digits.next()) {
        bytes.push((high.to_digit(16)? << 4 | low.to_digit(16)?) as u8);
    }
    String::from_utf8(bytes).ok().map(|text| (text, trailing))
}

/// Enumerates the application audio sessions Windows currently knows about.
///
/// Read-only, one fresh joined MTA worker per call, caller apartment unchanged. The
/// returned values are canonical [`AudioSource`]s; native observation records stay
/// private. See the module documentation for identity, ordering, bounds and the
/// non-atomic-snapshot limitation.
///
/// A successful empty list is a valid answer. Endpoint or session enumeration failures
/// are explicit errors and never become an empty success.
#[cfg(target_os = "windows")]
pub fn enumerate_application_audio_sources() -> Result<Vec<AudioSource>, RecorderError> {
    Ok(snapshot()?.into_sources())
}

/// Human-readable application-session diagnostics for the read-only probe.
///
/// Diagnostic text for a person to read, not a data API. It is produced from one
/// snapshot and states that snapshot's identity rules, counters, skipped/system/
/// multiprocess/unattributed records and resource bounds.
#[cfg(target_os = "windows")]
pub fn application_audio_diagnostics() -> Result<String, RecorderError> {
    Ok(render_diagnostics(&snapshot()?))
}

/// One application-session snapshot with its diagnostic records, for tooling.
///
/// This is the same single observation [`application_audio_diagnostics`] renders, offered
/// as values so a read-only probe can emit structured output instead of parsing text. It
/// is a diagnostic snapshot, not a domain model: it carries the canonical [`AudioSource`]
/// values plus the observation evidence behind them, and it is not wired into
/// `AudioBackend`, IPC or the frontend.
#[cfg(target_os = "windows")]
pub fn application_audio_session_snapshot() -> Result<ApplicationSessionSnapshot, RecorderError> {
    snapshot()
}

/// Observes and maps one snapshot through an injected observation source.
///
/// The single mapping path: production uses [`native::NativeApplicationSource`] on a
/// fresh joined MTA worker and the tests use a visibly synthetic source, so neither the
/// mapping rules nor the worker boundary is duplicated for tests.
pub(crate) fn snapshot_from(
    source: &(impl ApplicationObservationSource + Sync),
) -> Result<ApplicationSessionSnapshot, RecorderError> {
    map_observation(worker::run_joined(source)?)
}

/// The Windows-native snapshot: one fresh joined MTA worker per call.
#[cfg(target_os = "windows")]
pub(crate) fn snapshot() -> Result<ApplicationSessionSnapshot, RecorderError> {
    snapshot_from(&native::NativeApplicationSource)
}

/// Resolve private lifetime evidence for an exact selected live session. The full
/// observation still goes through the accepted mapper, including its failure/budget checks.
pub(crate) fn capture_identity(source: &AudioSourceId) -> Result<(u32, u64), RecorderError> {
    let (endpoint, instance) = decode_application_session_id(&source.0)
        .ok_or_else(|| RecorderError::invalid("Malformed application-session identity."))?;
    let observation = worker::run_joined(&native::NativeApplicationSource)?;
    let selected = observation
        .endpoints
        .iter()
        .filter(|item| item.endpoint_id == endpoint)
        .flat_map(|item| item.sessions.iter())
        .find(|item| item.instance_identifier.as_str() == instance);
    let identity = selected.map(|item| {
        (
            item.process_availability,
            item.process_id,
            item.process_creation_time,
            item.is_system_sounds,
            item.raw_state,
        )
    });
    let snapshot = map_observation(observation)?;
    let Some((availability, pid, creation, system, state)) = identity else {
        return Err(crate::application_audio::lost(
            "Selected session disappeared.",
        ));
    };
    crate::application_audio::validate_identity(
        availability,
        pid,
        creation,
        system,
        state,
        snapshot.sources.iter().any(|item| item.id == *source),
    )
}

/// Charges one retained native string after checking its per-string bound.
///
/// The bound is the ADR's UTF-16 **content** unit limit; a terminator, when the string
/// carries one, is charged separately by the caller that read it.
#[cfg(any(target_os = "windows", test))]
impl limits::SnapshotBudget {
    pub(crate) fn charge_native_string(
        &mut self,
        what: &str,
        units: usize,
    ) -> Result<(), RecorderError> {
        if units > limits::MAX_NATIVE_STRING_UTF16 {
            return Err(limits::string_limit_error(
                limits::MAX_NATIVE_STRING_UTF16,
                what,
                units,
            ));
        }
        // The owned UTF-8 form can be wider than two bytes per UTF-16 unit, so the
        // reservation uses the worst case of four bytes per unit plus a capacity header.
        self.reserve_capacity(units, 4, what)?;
        Ok(())
    }
}

/// Converts one observation into canonical sources plus diagnostic evidence.
///
/// This is the only place canonical values are produced. It continues charging the
/// ledger the collector started, so one monotonic total covers both stages, and it fails
/// on every recorded collection failure instead of reporting a shorter source list.
///
/// The seam is trusted only for *shape*: a fixture that reports an impossible bound
/// violation still fails here, because these checks are the mapper's own responsibility
/// and not a restatement of the collector's.
pub(crate) fn map_observation(
    observation: ObservationHeader,
) -> Result<ApplicationSessionSnapshot, RecorderError> {
    let ObservationHeader {
        endpoints: observed_endpoints,
        endpoints_reported,
        failures,
        mut budget,
    } = observation;

    // Every recorded collection failure is a snapshot failure: a pass that stopped early
    // must never be reported as a successful, shorter list.
    if let Some(failure) = failures.into_iter().next() {
        return Err(failure_into_error(failure));
    }

    // The mapper's own backstop for the count bounds, checked before it allocates
    // anything for the records.
    if endpoints_reported > budget.limits().max_endpoints_per_snapshot {
        return Err(limits::endpoint_limit_error(
            budget.limits().max_endpoints_per_snapshot,
            endpoints_reported,
        ));
    }

    let mut counters = ApplicationSessionCounters {
        endpoints_reported,
        ..ApplicationSessionCounters::default()
    };
    let mut endpoints: Vec<ApplicationEndpointRecord> = mapped_vec(
        &mut budget,
        observed_endpoints.len(),
        "mapped endpoint records",
    )?;
    // Validate the mapped shape independently. The incoming ledger has already
    // counted native items; mapping must not consume that allowance a second time.
    let mut mapped_counts = SnapshotBudget::with_limits(budget.limits());

    for (index, observed) in observed_endpoints.into_iter().enumerate() {
        mapped_counts.charge_endpoint(index)?;
        budget.charge_bytes(RESERVED_ENDPOINT_RECORD_BYTES, "an endpoint record")?;

        let endpoint_id = observed.endpoint_id;
        if endpoint_id.is_empty() {
            return Err(missing_endpoint_identity_error(&format!(
                "render endpoint #{} of this pass",
                index + 1
            )));
        }
        if !budget.native_owned() {
            budget.charge_native_string("the endpoint ID", endpoint_id.encode_utf16().count())?;
            charge_owned_string(&mut budget, "the endpoint ID", &endpoint_id)?;
        }

        let friendly_name = match observed.friendly_name {
            Some(name) => {
                let units = name.units();
                if !budget.native_owned() {
                    budget.charge_native_string("the endpoint friendly name", units)?;
                }
                Some(name.into_string())
            }
            None => None,
        };

        let device_summary = device_state_summary(observed.device_state);
        if observed.device_state & DEVICE_STATE_ACTIVE == 0 {
            counters.endpoints_inactive += 1;
            counters.endpoints_retained += 1;
            endpoints.push(ApplicationEndpointRecord {
                endpoint_id,
                friendly_name,
                device_state: observed.device_state,
                sessions: Vec::new(),
                issue: Some(ApplicationEndpointIssue::InactiveDeviceState {
                    state: observed.device_state,
                    summary: device_summary,
                }),
            });
            continue;
        }

        let mut sessions = mapped_vec(
            &mut budget,
            observed.sessions.len(),
            "mapped session records",
        )?;
        for (session_index, session) in observed.sessions.into_iter().enumerate() {
            // The mapper counts each session it is handed, including one whose own reads
            // failed, so a run of failures cannot hide behind the cap.
            mapped_counts.charge_session(session_index)?;
            counters.sessions_total += 1;
            sessions.push(map_session(
                &endpoint_id,
                session,
                &mut budget,
                &mut counters,
            )?);
        }

        counters.endpoints_retained += 1;
        endpoints.push(ApplicationEndpointRecord {
            endpoint_id,
            friendly_name,
            device_state: observed.device_state,
            sessions,
            issue: None,
        });
    }

    // Canonical ordering: by full source identity, which is endpoint ID then session
    // instance ID. It is independent of the order Windows enumerated in.
    // Duplicate identity is an explicit failure, not a deduplication: two records
    // claiming one live-session identity mean that identity cannot be trusted to select a
    // single session.
    let mut candidates = collect_candidates(&endpoints, &mut budget, &mut counters)?;
    candidates.sort_unstable_by(|a, b| a.id.cmp(&b.id));
    for pair in candidates.windows(2) {
        if pair[0].id == pair[1].id {
            return Err(duplicate_identity_error(&pair[0].id.0));
        }
    }

    counters.payload_bytes = budget.charged_bytes();
    Ok(ApplicationSessionSnapshot {
        sources: candidates,
        endpoints,
        counters,
        limits: budget.limits(),
    })
}

/// Charges one owned string at its exact byte length, after building it with that
/// capacity so the allocation cannot exceed what was reserved.
fn charge_owned_string(
    budget: &mut SnapshotBudget,
    what: &str,
    value: &str,
) -> Result<(), RecorderError> {
    budget.charge_bytes(value.len(), what)
}

fn mapped_vec<T>(
    budget: &mut SnapshotBudget,
    count: usize,
    what: &str,
) -> Result<Vec<T>, RecorderError> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let reserved = budget.reserve_capacity(count, std::mem::size_of::<T>(), what)?;
    let values = Vec::with_capacity(count);
    let actual = values.capacity() * std::mem::size_of::<T>();
    if actual > reserved {
        budget.charge_bytes(actual - reserved, what)?;
    }
    Ok(values)
}

fn mapped_string(
    budget: &mut SnapshotBudget,
    bytes: usize,
    what: &str,
) -> Result<String, RecorderError> {
    let reserved = budget.reserve_capacity(bytes, 1, what)?;
    let text = String::with_capacity(reserved);
    if text.capacity() > reserved {
        budget.charge_bytes(text.capacity() - reserved, what)?;
    }
    Ok(text)
}

/// Builds one canonical source per retained session, charging the ledger as it goes.
///
/// Sorting is a separate step so the duplicate-identity check can run over sorted
/// adjacency, which makes it independent of enumeration order and of which endpoint a
/// duplicate appeared on.
fn collect_candidates(
    endpoints: &[ApplicationEndpointRecord],
    budget: &mut SnapshotBudget,
    counters: &mut ApplicationSessionCounters,
) -> Result<Vec<AudioSource>, RecorderError> {
    let count = endpoints
        .iter()
        .map(|endpoint| endpoint.sessions.len())
        .sum();
    let mut candidates: Vec<AudioSource> = mapped_vec(budget, count, "canonical sources")?;
    for endpoint in endpoints {
        for session in &endpoint.sessions {
            if session.skip_reason.is_some() {
                continue;
            }
            let Some(id_ref) = session.source_id.as_ref() else {
                continue;
            };
            budget.reserve_capacity(id_ref.0.len(), 1, "a canonical source identity")?;
            let id = id_ref.clone();
            if id.0.capacity() > id_ref.0.len().next_power_of_two() {
                budget.charge_bytes(
                    id.0.capacity() - id_ref.0.len().next_power_of_two(),
                    "a canonical source identity",
                )?;
            }
            budget.charge_bytes(RESERVED_SOURCE_RECORD_BYTES, "a canonical source record")?;
            let display_name = source_label(session, budget)?;
            let app_identifier = if let Some(path) = &session.executable_path {
                let mut identifier = mapped_string(
                    budget,
                    APP_IDENTIFIER_PREFIX.len() + path.len(),
                    "a canonical application identifier",
                )?;
                identifier.push_str(APP_IDENTIFIER_PREFIX);
                identifier.push_str(path);
                Some(identifier)
            } else {
                None
            };
            if app_identifier.is_some() {
                counters.attributed_sources += 1;
            } else {
                counters.unattributed_sources += 1;
            }
            candidates.push(AudioSource {
                controls: Default::default(),
                capture_identity: None,
                muted: false,
                id,
                kind: AudioSourceKind::Application,
                display_name,
                process_id: session.process_id,
                app_identifier,
                is_active: session.is_active,
                route: AudioRoute::Ignore,
            });
        }
    }
    Ok(candidates)
}

/// Maps one raw session record into diagnostic-annotated form.
///
/// An essential read failure is a snapshot failure, except for the codes the OS documents
/// as "the session is gone", which become an explicit skip. Optional metadata failures
/// are recorded beside a retained source and never remove it.
fn map_session(
    endpoint_id: &str,
    session: ObservedSession,
    budget: &mut SnapshotBudget,
    counters: &mut ApplicationSessionCounters,
) -> Result<ApplicationSessionRecord, RecorderError> {
    budget.charge_bytes(RESERVED_SESSION_RECORD_BYTES, "a session record")?;
    if !budget.native_owned() {
        charge_native_string(budget, "a session identifier", &session.session_identifier)?;
        charge_native_string(
            budget,
            "a session instance identifier",
            &session.instance_identifier,
        )?;
    }
    if let Some(name) = &session.display_name {
        let units = name.units();
        if !budget.native_owned() {
            budget.charge_native_string("a session display name", units)?;
        }
    }
    if let Some(path) = &session.executable_path {
        let units = path.units();
        if !budget.native_owned() {
            budget.charge_native_string("an executable path", units)?;
        }
    }

    match session.process_availability {
        ApplicationProcessAvailability::SingleProcess => counters.sessions_single_process += 1,
        ApplicationProcessAvailability::MultiProcessSession => {
            counters.sessions_multiprocess += 1;
        }
        ApplicationProcessAvailability::Unavailable => counters.sessions_process_unavailable += 1,
        ApplicationProcessAvailability::SingleProcessZero => {
            counters.sessions_process_id_zero += 1;
        }
    }
    if session.process_creation_time.is_some() {
        counters.process_creation_times_observed += 1;
    }

    let mut metadata_failures = mapped_vec(
        budget,
        session.metadata_failures.len(),
        "mapped metadata failure records",
    )?;
    for failure in session.metadata_failures {
        // Public diagnostic records retain HRESULT separately; preserve a Win32
        // code in their text rather than silently dropping it at this boundary.
        let detail = if let Some(code) = failure.win32_code {
            let suffix = format!(" (Win32 error {code})");
            let mut detail = mapped_string(
                budget,
                failure.detail.len() + suffix.len(),
                "a metadata failure detail copy",
            )?;
            detail.push_str(&failure.detail);
            detail.push_str(&suffix);
            detail
        } else {
            failure.detail
        };
        budget.charge_bytes(RESERVED_METADATA_FAILURE_BYTES, "a metadata failure record")?;
        if !budget.native_owned() {
            charge_owned_string(budget, "a metadata failure operation", &failure.operation)?;
            if failure.win32_code.is_none() {
                charge_owned_string(budget, "a metadata failure detail", &detail)?;
            }
        }
        counters.metadata_reads_failed += 1;
        metadata_failures.push(ApplicationSessionReadFailure {
            operation: failure.operation,
            detail,
            hresult: failure.hresult,
        });
    }

    let session_identifier = session.session_identifier.into_string();
    let instance_identifier = session.instance_identifier.into_string();
    let display_name = session.display_name.map(WideString::into_string);
    let executable_path = session.executable_path.map(WideString::into_string);

    let mut record = ApplicationSessionRecord {
        source_id: None,
        session_identifier,
        instance_identifier,
        raw_state: session.raw_state,
        is_active: false,
        display_name,
        is_system_sounds: session.is_system_sounds,
        process_id: session.process_id,
        process_availability: session.process_availability,
        executable_path,
        executable_availability: session.executable_availability,
        metadata_failures,
        read_failure: None,
        skip_reason: None,
    };

    if let Some(failure) = session.essential_failure {
        let mut operation = mapped_string(
            budget,
            failure.operation.len(),
            "a session read failure operation copy",
        )?;
        operation.push_str(&failure.operation);
        let mut detail = mapped_string(
            budget,
            failure.detail.len(),
            "a session read failure detail copy",
        )?;
        detail.push_str(&failure.detail);
        let read_failure = ApplicationSessionReadFailure {
            operation,
            detail,
            hresult: failure.hresult,
        };
        record.read_failure = Some(read_failure);
        if !failure.recoverable_disappearance {
            // An unexpected essential failure is a snapshot failure: it must never become
            // a quietly missing session.
            return Err(essential_session_error(
                &record.session_identifier,
                &failure.operation,
                &failure.detail,
                failure.hresult,
                failure.win32_code,
            ));
        }
        counters.session_reads_failed += 1;
        record.skip_reason = Some(ApplicationSessionSkipReason::SessionReadFailed {
            operation: failure.operation,
            detail: failure.detail,
            hresult: failure.hresult,
        });
        return Ok(record);
    }

    // Expiry first: an expired system-sounds session is not a system-sounds decision.
    match session.raw_state {
        2 => {
            counters.sessions_expired += 1;
            record.skip_reason = Some(ApplicationSessionSkipReason::ExpiredSession);
            return Ok(record);
        }
        0 => {
            counters.sessions_inactive += 1;
            record.is_active = false;
        }
        1 => {
            counters.sessions_active += 1;
            record.is_active = true;
        }
        raw_state => {
            counters.sessions_unknown_state += 1;
            record.skip_reason =
                Some(ApplicationSessionSkipReason::UnknownSessionState { raw_state });
            return Ok(record);
        }
    }

    if session.is_system_sounds {
        counters.sessions_system_sounds += 1;
        record.skip_reason = Some(ApplicationSessionSkipReason::SystemSoundsSession);
        return Ok(record);
    }

    if record.session_identifier.is_empty() {
        counters.sessions_missing_identity += 1;
        record.skip_reason = Some(ApplicationSessionSkipReason::MissingSessionIdentity {
            missing: "session identifier",
        });
        return Ok(record);
    }
    if record.instance_identifier.is_empty() {
        counters.sessions_missing_identity += 1;
        record.skip_reason = Some(ApplicationSessionSkipReason::MissingSessionIdentity {
            missing: "session instance identifier",
        });
        return Ok(record);
    }

    let reserved = budget.reserve_capacity(
        session_id_capacity(endpoint_id, &record.instance_identifier),
        1,
        "a session identity",
    )?;
    let id = application_session_id(endpoint_id, &record.instance_identifier);
    if id.0.capacity() > reserved {
        budget.charge_bytes(id.0.capacity() - reserved, "a session identity")?;
    }
    if record.display_name.is_none() {
        if record.executable_path.is_some() {
            counters.sources_labelled_from_executable += 1;
        } else {
            counters.sources_labelled_unnamed += 1;
        }
    }
    record.source_id = Some(id);
    Ok(record)
}

/// Charges one retained native string, failing past the per-string bound.
fn charge_native_string(
    budget: &mut SnapshotBudget,
    what: &str,
    value: &WideString,
) -> Result<(), RecorderError> {
    budget.charge_native_string(what, value.units())
}

/// One recorded collection failure as a snapshot error.
fn failure_into_error(failure: worker::FailureEvent) -> RecorderError {
    let code = match (failure.hresult, failure.win32_code) {
        (Some(hresult), _) => format!("HRESULT 0x{:08X}", hresult as u32),
        (None, Some(win32)) => format!("Win32 error {win32}"),
        (None, None) => "no code".to_string(),
    };
    RecorderError {
        code: frame_contracts::ErrorCode::Internal,
        message: format!(
            "Windows application audio session discovery failed: {} failed with {code} ({}). No snapshot is reported instead of a fabricated empty one.",
            failure.operation, failure.detail
        ),
        recovery: frame_contracts::RecoveryStatus::NoRecording,
        action: "Retry application-session discovery; if it persists, report the Windows build and audio device."
            .to_string(),
    }
}

/// One unexpected essential session read failure as a snapshot error.
fn essential_session_error(
    session_identifier: &str,
    operation: &str,
    detail: &str,
    hresult: Option<i32>,
    win32_code: Option<u32>,
) -> RecorderError {
    let code = match (hresult, win32_code) {
        (Some(hresult), _) => format!("HRESULT 0x{:08X}", hresult as u32),
        (None, Some(win32)) => format!("Win32 error {win32}"),
        (None, None) => "no code".to_string(),
    };
    let session = if session_identifier.is_empty() {
        "an unnamed session".to_string()
    } else {
        format!("session {session_identifier}")
    };
    RecorderError {
        code: frame_contracts::ErrorCode::Internal,
        message: format!(
            "Windows application audio session discovery failed: reading {operation} failed for {session} with {code} ({detail}). No snapshot is reported instead of a quietly missing session."
        ),
        recovery: frame_contracts::RecoveryStatus::NoRecording,
        action: "Retry application-session discovery; if it persists, report the Windows build and the applications producing audio."
            .to_string(),
    }
}

/// The honest label for a source.
///
/// Preference order: the OS display name; the executable basename, which is real
/// evidence from the OS rather than an invented application name; the literal
/// unnamed-session label. Never a synthesized application identity.
fn source_label(
    session: &ApplicationSessionRecord,
    budget: &mut SnapshotBudget,
) -> Result<String, RecorderError> {
    let label = session
        .display_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .or_else(|| {
            session
                .executable_path
                .as_deref()
                .and_then(executable_basename)
        })
        .unwrap_or(UNNAMED_SESSION_LABEL);
    let mut owned = mapped_string(budget, label.len(), "a canonical source name")?;
    owned.push_str(label);
    Ok(owned)
}

/// The final path component of an executable path, when it has one.
fn executable_basename(path: &str) -> Option<&str> {
    let base = path
        .rsplit(['\\', '/'])
        .next()
        .map(str::trim)
        .filter(|base| !base.is_empty())?;
    Some(base)
}

/// Where a session record's in-progress identity was empty.
fn device_state_summary(state: u32) -> &'static str {
    match state {
        0 => "inactive",
        1 => "active",
        2 => "disabled",
        4 => "not present",
        8 => "unplugged",
        _ => "an unrecognised device state",
    }
}

/// A malformed observation from the seam, which the production source cannot produce.
fn missing_endpoint_identity_error(label: &str) -> RecorderError {
    RecorderError {
        code: frame_contracts::ErrorCode::Internal,
        message: format!(
            "Session discovery received an endpoint observation without an endpoint ID ({label}); identity is never invented for an endpoint."
        ),
        recovery: frame_contracts::RecoveryStatus::NoRecording,
        action: "Retry application-session discovery; if it persists, report the error."
            .to_string(),
    }
}

/// Two records claimed one live-session identity in a single snapshot.
fn duplicate_identity_error(id: &str) -> RecorderError {
    RecorderError {
        code: frame_contracts::ErrorCode::Internal,
        message: format!(
            "Two separate session records in one snapshot claim the identity {id}, so that identity cannot reliably select a session."
        ),
        recovery: frame_contracts::RecoveryStatus::NoRecording,
        action: "Retry application-session discovery; if it persists, report the Windows build and the applications producing audio."
            .to_string(),
    }
}

/// The device-state active flag from `MMDeviceAPI.h`, kept as a plain value so the
/// mapping rules stay platform independent.
pub(crate) const DEVICE_STATE_ACTIVE: u32 = 1;

/// Renders the human-readable diagnostics report for one snapshot.
fn render_diagnostics(snapshot: &ApplicationSessionSnapshot) -> String {
    let mut report = String::new();
    report.push_str("Frame application audio session diagnostics (FRAME-0106A)\n\n");
    for line in [
        "This is a read-only diagnostic snapshot of Windows application audio sessions.",
        "It captures no audio, changes no system setting and starts no program. It does NOT",
        "provide process-isolated audio capture: that boundary belongs to the deferred",
        "FRAME-0106B and is not implemented.",
        "",
        "The snapshot is not atomic. Each call enumerates again, and Windows performs these",
        "reads on live objects, so a session created, destroyed or moved between two reads",
        "can be missing here or described by an older value. This is not a live tracker and",
        "there is no change notification behind it.",
        "",
        "A source ID is a live-session selection identity, not a durable route rule for an",
        "application: it changes when the session ends and a new one starts, even for the",
        "same executable. A display name is never identity, and several sessions can share",
        "one name or one executable path.",
        "",
        "is_active is the OS session state (AudioSessionStateActive). It is not microphone",
        "permission, not proof that audio is flowing and not process-loopback availability.",
    ] {
        report.push_str(line);
        report.push('\n');
    }

    let limits = snapshot.limits;
    report.push_str(&format!(
        "\nBounds (exceeding any of these fails the snapshot, it never truncates): {} endpoints, {} sessions, {} UTF-16 units per native string, {} bytes of Frame-owned snapshot payload. This snapshot charged {} bytes across {} endpoints and {} sessions.\n",
        limits.max_endpoints_per_snapshot,
        limits.max_sessions_per_snapshot,
        limits.max_native_string_utf16,
        limits.max_snapshot_payload_bytes,
        snapshot.counters.payload_bytes,
        snapshot.counters.endpoints_retained,
        snapshot.counters.sessions_total,
    ));

    let counters = snapshot.counters;
    report.push_str("\nCounters\n");
    for (name, value) in [
        ("endpoints reported", counters.endpoints_reported),
        ("endpoints retained", counters.endpoints_retained),
        (
            "endpoints inactive (no sources)",
            counters.endpoints_inactive,
        ),
        ("sessions total", counters.sessions_total),
        ("sessions active", counters.sessions_active),
        ("sessions inactive (retained)", counters.sessions_inactive),
        ("sessions expired (excluded)", counters.sessions_expired),
        (
            "sessions unknown state (excluded)",
            counters.sessions_unknown_state,
        ),
        (
            "sessions system sounds (excluded)",
            counters.sessions_system_sounds,
        ),
        (
            "sessions ended during the pass (excluded)",
            counters.session_reads_failed,
        ),
        (
            "sessions missing identity (excluded)",
            counters.sessions_missing_identity,
        ),
        (
            "optional metadata reads failed (source retained)",
            counters.metadata_reads_failed,
        ),
        (
            "process creation times observed (private)",
            counters.process_creation_times_observed,
        ),
        ("sessions single process", counters.sessions_single_process),
        (
            "sessions multiprocess (no single pid)",
            counters.sessions_multiprocess,
        ),
        (
            "sessions process id zero",
            counters.sessions_process_id_zero,
        ),
        (
            "sessions process unavailable",
            counters.sessions_process_unavailable,
        ),
        ("sources attributed (win-exe:)", counters.attributed_sources),
        (
            "sources unattributed (no app_identifier)",
            counters.unattributed_sources,
        ),
        (
            "sources labelled from executable",
            counters.sources_labelled_from_executable,
        ),
        (
            "sources labelled unnamed session",
            counters.sources_labelled_unnamed,
        ),
    ] {
        report.push_str(&format!("  {name}: {value}\n"));
    }

    if snapshot.sources.is_empty() {
        report.push_str(
            "\nSources: none. A real empty list is a valid result for this snapshot. It is not proof that discovery works, and it is not proof that sessions were missed: read the counters and the per-endpoint evidence below.\n",
        );
    } else {
        report.push_str(&format!("\nSources ({}):\n", snapshot.sources.len()));
        for (index, source) in snapshot.sources.iter().enumerate() {
            report.push_str(&format!("{}. {}\n", index + 1, source.display_name));
            report.push_str(&format!("   id: {}\n", source.id.0));
            if let Some((endpoint_id, instance_id)) = decode_application_session_id(&source.id.0) {
                report.push_str(&format!("   endpoint id: {endpoint_id}\n"));
                report.push_str(&format!("   session instance id: {instance_id}\n"));
            }
            report.push_str(&format!(
                "   kind: {:?}  route: {:?}  active: {}\n",
                source.kind, source.route, source.is_active
            ));
            match source.process_id {
                Some(pid) => report.push_str(&format!("   process id: {pid}\n")),
                None => report.push_str(
                    "   process id: none (no single process owns the session: it may span more than one process, report no usable process, or its owner could not be read; see the records below)\n",
                ),
            }
            match &source.app_identifier {
                Some(identifier) => {
                    report.push_str(&format!("   app identifier: {identifier}\n"));
                }
                None => report.push_str(
                    "   app identifier: none (no readable executable path; no name, path or PID is invented)\n",
                ),
            }
        }
    }

    report.push_str("\nEndpoints and session records (raw evidence)\n");
    for (index, endpoint) in snapshot.endpoints().iter().enumerate() {
        report.push_str(&format!(
            "{}. {}\n   state: {} ({})\n",
            index + 1,
            endpoint.endpoint_id,
            endpoint.device_state,
            device_state_summary(endpoint.device_state)
        ));
        match &endpoint.friendly_name {
            Some(name) => report.push_str(&format!("   name: {name}\n")),
            None => report.push_str("   name: none reported by the OS\n"),
        }
        if let Some(issue) = &endpoint.issue {
            report.push_str(&format!(
                "   problem [{}]: {}\n",
                issue.category(),
                describe_endpoint_issue(issue)
            ));
        }
        if endpoint.sessions.is_empty() && endpoint.issue.is_none() {
            report.push_str("   no sessions were reported on this endpoint in this snapshot\n");
        }
        for (session_index, session) in endpoint.sessions.iter().enumerate() {
            report.push_str(&format!(
                "   session {}: state {} (active: {})\n",
                session_index + 1,
                session.raw_state,
                session.is_active
            ));
            report.push_str(&format!(
                "      session identifier: {}\n",
                quoted(&session.session_identifier)
            ));
            report.push_str(&format!(
                "      session instance identifier: {}\n",
                quoted(&session.instance_identifier)
            ));
            match &session.display_name {
                Some(name) => report.push_str(&format!("      display name: {name}\n")),
                None => report.push_str(
                    "      display name: none reported by the OS (the label falls back to the executable basename or the literal unnamed-session label, never an invented name)\n",
                ),
            }
            report.push_str(&format!(
                "      system sounds: {}  process: {}  executable path: {}\n",
                session.is_system_sounds,
                session.process_availability.category(),
                session.executable_availability.category()
            ));
            match session.process_id {
                Some(pid) => report.push_str(&format!("      process id: {pid}\n")),
                None => report.push_str("      process id: none\n"),
            }
            if let Some(path) = &session.executable_path {
                report.push_str(&format!("      executable path: {path}\n"));
            }
            for failure in &session.metadata_failures {
                report.push_str(&format!(
                    "      optional metadata unavailable: {} ({}{}) - the session is kept and no identifier is invented\n",
                    failure.operation,
                    failure.detail,
                    failure
                        .hresult
                        .map(|code| format!(", HRESULT 0x{code:08X}"))
                        .unwrap_or_default()
                ));
            }
            if let Some(failure) = &session.read_failure {
                report.push_str(&format!(
                    "      essential read failure: {} ({}{})\n",
                    failure.operation,
                    failure.detail,
                    failure
                        .hresult
                        .map(|code| format!(", HRESULT 0x{code:08X}"))
                        .unwrap_or_default()
                ));
            }
            if let Some(reason) = &session.skip_reason {
                report.push_str(&format!(
                    "      skipped [{}]: {}\n",
                    reason.category(),
                    describe_skip_reason(reason)
                ));
            } else {
                report.push_str("      retained as a source\n");
            }
        }
    }
    report
}

/// Renders one skip reason as a sentence.
fn describe_skip_reason(reason: &ApplicationSessionSkipReason) -> String {
    match reason {
        ApplicationSessionSkipReason::SystemSoundsSession => {
            "IsSystemSoundsSession returned S_OK, so this is a system sounds session, not an application source."
                .to_string()
        }
        ApplicationSessionSkipReason::ExpiredSession => {
            "the session state is AudioSessionStateExpired, so the session is gone.".to_string()
        }
        ApplicationSessionSkipReason::SessionReadFailed {
            operation,
            detail,
            hresult,
        } => format!(
            "reading {operation} failed ({detail}{}), so this record is partial and unusable; unrelated sessions are unaffected.",
            hresult
                .map(|code| format!(", HRESULT 0x{code:08X}"))
                .unwrap_or_default()
        ),
        ApplicationSessionSkipReason::MissingSessionIdentity { missing } => format!(
            "Windows reported an empty {missing}, and no identity is invented for a session."
        ),
        ApplicationSessionSkipReason::UnknownSessionState { raw_state } => format!(
            "the session reported state {raw_state}, which is none of the three documented AudioSessionState values, so no is_active value is guessed."
        ),
    }
}

/// Renders one endpoint issue as a sentence.
fn describe_endpoint_issue(issue: &ApplicationEndpointIssue) -> String {
    match issue {
        ApplicationEndpointIssue::InactiveDeviceState { state, summary } => format!(
            "the endpoint reported device state {state} ({summary}) during this pass, so it is not an active render endpoint and its sessions were not treated as application sources."
        ),
        ApplicationEndpointIssue::EndpointReadFailed {
            operation,
            detail,
            hresult,
            ..
        } => format!(
            "reading {operation} failed ({detail}{}); this endpoint's sessions are unknown, and sessions of other endpoints are still reported.",
            hresult
                .map(|code| format!(", HRESULT 0x{code:08X}"))
                .unwrap_or_default()
        ),
    }
}

/// Quotes a possibly empty OS string so an empty value is visible in the report.
fn quoted(value: &str) -> String {
    if value.is_empty() {
        "<empty>".to_string()
    } else {
        format!("\"{value}\"")
    }
}
