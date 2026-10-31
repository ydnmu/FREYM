// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Deterministic unit tests for application audio session discovery (FRAME-0106A).
//!
//! These tests drive the production mapping rules and the production joined-worker
//! boundary through the private [`ApplicationObservationSource`] seam with a visibly
//! synthetic source. They need no Windows audio session, no application producing audio
//! and no hardware, and none of them is hardware evidence: the real-machine probe is
//! checked separately on real devices, and the native reader's own failures are
//! covered by the scripted collection seam inside `native`.
//!
//! What is *not* proven here is anything that depends on real COM objects.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use frame_contracts::{AudioRoute, AudioSourceKind, ErrorCode, RecorderError, RecoveryStatus};

use super::limits::{
    MAX_ENDPOINTS_PER_SNAPSHOT, MAX_NATIVE_STRING_UTF16, MAX_SESSIONS_PER_SNAPSHOT,
    MAX_SNAPSHOT_PAYLOAD_BYTES, SnapshotBudget,
};
use super::worker::{
    ApplicationObservationSource, EssentialSessionFailure, FailureEvent, MetadataFailure,
    ObservationHeader, ObservedEndpoint, ObservedSession, SessionFailureKind, WideString,
};
use super::{
    APPLICATION_SESSION_LIMITS, ApplicationEndpointIssue, ApplicationExecutableAvailability,
    ApplicationProcessAvailability, ApplicationProcessIdStatus, ApplicationSessionSkipReason,
    ApplicationSystemSoundsStatus, classify_process_id_status, classify_system_sounds_status,
    decode_application_session_id, render_diagnostics, snapshot_from,
};

/// The synthetic observation source: it *reports* what a pass saw, and never re-implements
/// a mapping or identity rule.
enum SyntheticSource {
    /// A successful pass with this observation.
    Observed(ObservationHeader),
    /// A pass that could not be taken at all.
    Failed(RecorderError),
    /// A pass that panicked inside the worker.
    Panics,
    /// A pass that counts how many times it ran, to prove one worker per call.
    Counting(Arc<AtomicUsize>, ObservationHeader),
}

impl ApplicationObservationSource for SyntheticSource {
    fn observe_application_sessions(&self) -> Result<ObservationHeader, RecorderError> {
        match self {
            Self::Observed(header) => Ok(header.clone()),
            Self::Failed(error) => Err(error.clone()),
            Self::Panics => panic!("synthetic session observation panicked"),
            Self::Counting(count, header) => {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(header.clone())
            }
        }
    }
}

/// Runs one synthetic pass and returns the snapshot.
fn run_snapshot(source: &SyntheticSource) -> super::ApplicationSessionSnapshot {
    match snapshot_from(source) {
        Ok(snapshot) => snapshot,
        Err(error) => panic!(
            "the synthetic pass must map, got {:?}: {}",
            error.code, error.message
        ),
    }
}

/// Runs one synthetic pass that must fail, and returns the error.
fn snapshot_error(source: &SyntheticSource) -> RecorderError {
    match snapshot_from(source) {
        Err(error) => error,
        Ok(snapshot) => panic!(
            "the synthetic pass must fail, got {} source(s): {:?}",
            snapshot.sources().len(),
            snapshot
                .sources()
                .iter()
                .map(|s| s.id.0.clone())
                .collect::<Vec<_>>()
        ),
    }
}

/// A session record with a single process and no display name.
fn session(instance: &str) -> ObservedSession {
    session_with_identity("session-id", instance)
}

/// A session record with the given identifiers.
fn session_with_identity(session_identifier: &str, instance: &str) -> ObservedSession {
    ObservedSession {
        session_identifier: wide(session_identifier),
        instance_identifier: wide(instance),
        raw_state: 1,
        display_name: None,
        is_system_sounds: false,
        process_id: Some(1000),
        process_availability: ApplicationProcessAvailability::SingleProcess,
        executable_path: Some(wide(r"C:\Program Files\Synthetic\app.exe")),
        executable_availability: ApplicationExecutableAvailability::Available,
        process_creation_time: Some(133_000_000_000_000_000),
        essential_failure: None,
        metadata_failures: Vec::new(),
    }
}

/// A [`WideString`] whose unit count is its UTF-16 code-unit count.
fn wide(text: &str) -> WideString {
    WideString::new(text.to_string(), text.encode_utf16().count())
}

/// A [`WideString`] with an explicit unit count, so a bound can be crossed without
/// allocating a genuinely huge string.
fn wide_with_units(text: &str, units: usize) -> WideString {
    WideString::new(text.to_string(), units)
}

/// An endpoint record carrying the given sessions.
fn endpoint(endpoint_id: &str, sessions: Vec<ObservedSession>) -> ObservedEndpoint {
    ObservedEndpoint {
        endpoint_id: endpoint_id.to_string(),
        friendly_name: Some(wide("Synthetic render endpoint")),
        device_state: 1,
        sessions,
    }
}

/// An observation header with one endpoint.
fn observation(endpoint_id: &str, sessions: Vec<ObservedSession>) -> ObservationHeader {
    header(vec![endpoint(endpoint_id, sessions)])
}

/// An observation header with the given endpoints.
fn header(endpoints: Vec<ObservedEndpoint>) -> ObservationHeader {
    let reported = endpoints.len();
    ObservationHeader::fixture(endpoints, reported)
}

/// An observation header that records collection failures.
fn failed_header(
    endpoints: Vec<ObservedEndpoint>,
    failures: Vec<FailureEvent>,
) -> ObservationHeader {
    let reported = endpoints.len();
    ObservationHeader::with_failures(endpoints, reported, failures, SnapshotBudget::new())
}

/// The canonical source IDs of a snapshot, in reported order.
fn ids(snapshot: &super::ApplicationSessionSnapshot) -> Vec<String> {
    snapshot
        .sources()
        .iter()
        .map(|source| source.id.0.clone())
        .collect()
}

/// The endpoint ID used by most tests.
const ENDPOINT: &str =
    r"\\?\SWD#MMDEVAPI#{0.0.0.00000000}.{synthetic}#{e6327cad-dcec-4949-ae8a-991e976a79d2}";

// ---------------------------------------------------------------------------
// Identity encoding
// ---------------------------------------------------------------------------

/// The identity is the documented prefix plus two length-prefixed components, and it
/// round-trips back to exactly the two OS values it was built from.
#[test]
fn the_identity_encoding_is_length_prefixed_and_round_trips() {
    let id = super::application_session_id(ENDPOINT, "{instance-1}");
    assert!(id.0.starts_with("wasapi-session:"));

    // `<bytes>:<hex>:<bytes>:<hex>:` for both components; the trailing colon of the last
    // component is its terminator, so the hex payload stops before it.
    let encoded = id.0.strip_prefix("wasapi-session:").expect("prefix");
    let (endpoint_units, rest) = encoded.split_once(':').expect("endpoint length");
    assert_eq!(endpoint_units.parse::<usize>().unwrap(), ENDPOINT.len());
    let (endpoint_hex, rest) = rest.split_once(':').expect("endpoint hex");
    assert_eq!(endpoint_hex.len(), ENDPOINT.len() * 2);
    assert!(endpoint_hex.chars().all(|c| c.is_ascii_hexdigit()));
    let (instance_units, rest) = rest.split_once(':').expect("instance length");
    assert_eq!(
        instance_units.parse::<usize>().unwrap(),
        "{instance-1}".len()
    );
    let instance_hex = rest.expect_terminator();
    assert_eq!(instance_hex.len(), "{instance-1}".len() * 2);
    assert!(instance_hex.chars().all(|c| c.is_ascii_hexdigit()));

    assert_eq!(
        decode_application_session_id(&id.0),
        Some((ENDPOINT.to_string(), "{instance-1}".to_string()))
    );
}

/// Both components are length-prefixed, so no value can be re-parsed out of a
/// concatenation: moving bytes between the two components produces a different identity,
/// not the same one.
#[test]
fn the_length_prefixes_make_the_identity_unambiguous() {
    let split_one = super::application_session_id("ab", "cde");
    let split_two = super::application_session_id("abc", "de");
    assert_ne!(split_one, split_two);
    assert_eq!(
        decode_application_session_id(&split_one.0),
        Some(("ab".to_string(), "cde".to_string()))
    );
    assert_eq!(
        decode_application_session_id(&split_two.0),
        Some(("abc".to_string(), "de".to_string()))
    );

    // A separator-like value inside a component cannot be confused with framing.
    let separator = super::application_session_id("a:2:6262:", "c");
    assert_eq!(
        decode_application_session_id(&separator.0),
        Some(("a:2:6262:".to_string(), "c".to_string()))
    );
}

/// Distinct session instances stay distinct even when nothing else about them differs,
/// and separate endpoints stay distinguishable.
#[test]
fn distinct_instances_and_endpoints_stay_distinguishable() {
    let one = super::application_session_id(ENDPOINT, "instance-1");
    let two = super::application_session_id(ENDPOINT, "instance-2");
    assert_ne!(one, two);
    let other_endpoint = super::application_session_id("other-endpoint", "instance-1");
    assert_ne!(one, other_endpoint);
    assert_eq!(
        one,
        super::application_session_id(ENDPOINT, "instance-1"),
        "the same session values must produce the same identity"
    );
}

/// A malformed identity is reported as malformed rather than parsed partially.
#[test]
fn a_malformed_identity_does_not_decode() {
    for malformed in [
        "",
        "mic:1:61:",
        "wasapi-session:",
        "wasapi-session:x:61:",
        "wasapi-session:2:6:",
        "wasapi-session:2:6z:1:61:",
        "wasapi-session:2:6162:1:63:extra",
        "wasapi-session:2:61:1:63:",
    ] {
        assert_eq!(
            decode_application_session_id(malformed),
            None,
            "{malformed:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Ordering and duplicate identity
// ---------------------------------------------------------------------------

/// Ordering is by full identity, independent of the order Windows enumerated in.
#[test]
fn ordering_is_deterministic_for_shuffled_native_inputs() {
    let shuffled = observation(
        ENDPOINT,
        vec![
            session("zzz"),
            session("aaa"),
            session("mmm"),
            session("000"),
        ],
    );
    let mut reversed_sessions = shuffled.clone();
    reversed_sessions.endpoints[0].sessions.reverse();
    let mut reversed_endpoints = header(vec![
        endpoint("endpoint-b", vec![session("b-2"), session("b-1")]),
        endpoint("endpoint-a", vec![session("a-2"), session("a-1")]),
    ]);
    let mut endpoints_reversed = reversed_endpoints.clone();
    endpoints_reversed.endpoints.reverse();

    let first = ids(&run_snapshot(&SyntheticSource::Observed(shuffled.clone())));
    let second = ids(&run_snapshot(&SyntheticSource::Observed(reversed_sessions)));
    assert_eq!(first, second, "shuffled sessions must order the same way");

    reversed_endpoints.endpoints.reverse();
    let third = ids(&run_snapshot(&SyntheticSource::Observed(
        reversed_endpoints,
    )));
    let fourth = ids(&run_snapshot(&SyntheticSource::Observed(
        endpoints_reversed,
    )));
    assert_eq!(third, fourth, "shuffled endpoints must order the same way");
    assert!(third[0] < third[1] && third[2] < third[3]);
    assert!(third[0].contains("endpoint-a") || third[0] < third[2]);
}

/// Two sessions that share a display name, an executable path and a pid but differ in
/// session instance are two sources, not one.
#[test]
fn duplicate_display_names_do_not_collide() {
    let mut one = session("instance-1");
    one.display_name = Some(wide("Synthetic App"));
    let mut two = session("instance-2");
    two.display_name = Some(wide("Synthetic App"));
    two.process_id = Some(1000);
    let snapshot = run_snapshot(&SyntheticSource::Observed(observation(
        ENDPOINT,
        vec![one, two],
    )));
    assert_eq!(snapshot.sources().len(), 2);
    assert_ne!(snapshot.sources()[0].id, snapshot.sources()[1].id);
    assert_eq!(
        snapshot.sources()[0].display_name,
        snapshot.sources()[1].display_name
    );
    assert_eq!(
        snapshot.sources()[0].app_identifier,
        snapshot.sources()[1].app_identifier
    );
}

/// One live-session identity claimed by two records is an explicit snapshot failure, not a
/// deduplication.
#[test]
fn a_duplicate_identity_fails_the_snapshot() {
    let error = snapshot_error(&SyntheticSource::Observed(observation(
        ENDPOINT,
        vec![session("same"), session("same")],
    )));
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(
        error.message.contains("wasapi-session:"),
        "{}",
        error.message
    );
    assert!(error.message.contains("identity"), "{}", error.message);
    // A duplicate of one endpoint/instance pair, reported twice by the same endpoint
    // record, is refused too: adjacency after sorting is what makes this order
    // independent.
    let duplicated = session_with_identity("shared", "shared-instance");
    let twice = snapshot_error(&SyntheticSource::Observed(observation(
        ENDPOINT,
        vec![duplicated.clone(), session("other"), duplicated],
    )));
    assert_eq!(twice.code, ErrorCode::Internal);
    assert!(twice.message.contains("identity"), "{}", twice.message);

    // The identity is endpoint-scoped: the same session and instance values on two
    // endpoints are two different live sessions, not a duplicate.
    let two_endpoints = run_snapshot(&SyntheticSource::Observed(header(vec![
        endpoint(
            "endpoint-a",
            vec![session_with_identity("shared", "shared-instance")],
        ),
        endpoint(
            "endpoint-b",
            vec![session_with_identity("shared", "shared-instance")],
        ),
    ])));
    assert_eq!(two_endpoints.sources().len(), 2);
    assert_ne!(two_endpoints.sources()[0].id, two_endpoints.sources()[1].id);
}

/// Repeated enumeration of the same live session preserves its ID.
#[test]
fn repeated_enumeration_preserves_the_session_identity() {
    let first = ids(&run_snapshot(&SyntheticSource::Observed(observation(
        ENDPOINT,
        vec![session("instance-1")],
    ))));
    let second = ids(&run_snapshot(&SyntheticSource::Observed(observation(
        ENDPOINT,
        vec![session("instance-1")],
    ))));
    assert_eq!(first, second);
    assert_eq!(first.len(), 1);
}

// ---------------------------------------------------------------------------
// Activity, system sounds and process statuses
// ---------------------------------------------------------------------------

/// `is_active` is the OS session state, retained exactly, with inactive sessions kept.
#[test]
fn activity_follows_the_os_session_state() {
    let mut active = session("first");
    active.raw_state = 1;
    active.executable_path = Some(wide(r"C:\Synthetic\active.exe"));
    let mut inactive = session("second");
    inactive.raw_state = 0;
    inactive.executable_path = Some(wide(r"C:\Synthetic\inactive.exe"));
    let snapshot = run_snapshot(&SyntheticSource::Observed(observation(
        ENDPOINT,
        vec![active, inactive],
    )));
    assert_eq!(
        snapshot.sources().len(),
        2,
        "inactive sessions are retained"
    );
    assert_eq!(
        snapshot.counters().sessions_active,
        1,
        "exactly one session reported the active OS state"
    );
    assert_eq!(snapshot.counters().sessions_inactive, 1);
    assert_eq!(snapshot.counters().sessions_expired, 0);
    // The value follows the OS state per session, not a snapshot-wide guess.
    for source in snapshot.sources() {
        let identifier = source.app_identifier.as_deref().expect("an identifier");
        let basename = identifier.rsplit('\\').next().expect("a basename");
        assert_eq!(source.is_active, basename == "active.exe", "{identifier}");
    }
}

/// An expired session is excluded with its own counter and never reported as inactive.
#[test]
fn expired_sessions_are_excluded_and_counted() {
    let mut expired = session("expired");
    expired.raw_state = 2;
    let snapshot = run_snapshot(&SyntheticSource::Observed(observation(
        ENDPOINT,
        vec![expired, session("kept")],
    )));
    assert_eq!(snapshot.sources().len(), 1);
    assert_eq!(snapshot.counters().sessions_expired, 1);
    assert_eq!(snapshot.counters().sessions_total, 2);
    let record = &snapshot.endpoints()[0].sessions[0];
    assert_eq!(
        record.skip_reason,
        Some(ApplicationSessionSkipReason::ExpiredSession)
    );
}

/// A session state outside the three documented values is excluded rather than guessed
/// into `is_active`.
#[test]
fn an_unknown_session_state_is_excluded_not_guessed() {
    let mut unknown = session("unknown");
    unknown.raw_state = 7;
    let snapshot = run_snapshot(&SyntheticSource::Observed(observation(
        ENDPOINT,
        vec![unknown],
    )));
    assert!(snapshot.sources().is_empty());
    assert_eq!(snapshot.counters().sessions_unknown_state, 1);
    assert_eq!(
        snapshot.endpoints()[0].sessions[0].skip_reason,
        Some(ApplicationSessionSkipReason::UnknownSessionState { raw_state: 7 })
    );
}

/// `IsSystemSoundsSession` `S_OK` means system sounds and `S_FALSE`, also a success, means
/// it is not. Both must be honored: the second is every real application.
#[test]
fn system_sounds_s_ok_and_s_false_are_distinguished() {
    assert_eq!(
        classify_system_sounds_status(0),
        ApplicationSystemSoundsStatus::IsSystemSounds
    );
    assert_eq!(
        classify_system_sounds_status(1),
        ApplicationSystemSoundsStatus::IsNotSystemSounds
    );
    assert_eq!(
        classify_system_sounds_status(0x8007_0005_u32 as i32),
        ApplicationSystemSoundsStatus::Failed {
            hresult: 0x8007_0005_u32 as i32
        }
    );

    let mut system = session("system");
    system.is_system_sounds = true;
    let mut application = session("application");
    application.is_system_sounds = false;
    let snapshot = run_snapshot(&SyntheticSource::Observed(observation(
        ENDPOINT,
        vec![system, application],
    )));
    assert_eq!(
        snapshot.sources().len(),
        1,
        "only the application is a source"
    );
    assert_eq!(snapshot.counters().sessions_system_sounds, 1);
    assert_eq!(
        snapshot.endpoints()[0].sessions[0].skip_reason,
        Some(ApplicationSessionSkipReason::SystemSoundsSession)
    );
}

/// `GetProcessId` `S_OK` with one process, `S_OK` with a zero identity,
/// `AUDCLNT_S_NO_SINGLE_PROCESS` and a failure are four different outcomes.
#[test]
fn process_id_statuses_are_classified_separately() {
    const NO_SINGLE: i32 = 0x0889_000D_u32 as i32;
    assert_eq!(
        classify_process_id_status(0, true, 4242),
        ApplicationProcessIdStatus::SingleProcess { process_id: 4242 }
    );
    assert_eq!(
        classify_process_id_status(0, true, 0),
        ApplicationProcessIdStatus::SingleProcessZero
    );
    assert_eq!(
        classify_process_id_status(NO_SINGLE, true, 4242),
        ApplicationProcessIdStatus::NoSingleProcess,
        "the documented positive status is a success, not a failure"
    );
    assert_eq!(
        classify_process_id_status(NO_SINGLE, false, 0),
        ApplicationProcessIdStatus::NoSingleProcess
    );
    assert_eq!(
        classify_process_id_status(0x8889_0004_u32 as i32, false, 0),
        ApplicationProcessIdStatus::Failed {
            hresult: 0x8889_0004_u32 as i32
        }
    );
    assert_eq!(
        classify_process_id_status(0, false, 0),
        ApplicationProcessIdStatus::Failed { hresult: 0 },
        "success without an out-value is not a process"
    );
}

/// A multiprocess session gets no single `process_id`, no executable path and a
/// diagnostic reason, rather than the initial creator's identifier masquerading as the
/// sole owner.
#[test]
fn a_multiprocess_session_has_no_single_process_id() {
    let mut multiprocess = session("multiprocess");
    multiprocess.process_id = None;
    multiprocess.process_availability = ApplicationProcessAvailability::MultiProcessSession;
    multiprocess.executable_path = None;
    multiprocess.executable_availability = ApplicationExecutableAvailability::NoSingleProcess;
    let snapshot = run_snapshot(&SyntheticSource::Observed(observation(
        ENDPOINT,
        vec![multiprocess],
    )));
    assert_eq!(snapshot.sources().len(), 1, "the session is still a source");
    assert_eq!(snapshot.sources()[0].process_id, None);
    assert_eq!(snapshot.sources()[0].app_identifier, None);
    assert_eq!(snapshot.counters().sessions_multiprocess, 1);
    assert_eq!(snapshot.counters().unattributed_sources, 1);
    assert_eq!(
        snapshot.endpoints()[0].sessions[0].executable_availability,
        ApplicationExecutableAvailability::NoSingleProcess
    );
}

/// A single process identity of zero is not attributed as a process.
#[test]
fn a_zero_process_identity_is_not_a_process() {
    let mut zero = session("zero");
    zero.process_id = None;
    zero.process_availability = ApplicationProcessAvailability::SingleProcessZero;
    zero.executable_path = None;
    zero.executable_availability = ApplicationExecutableAvailability::ProcessUnavailable;
    let snapshot = run_snapshot(&SyntheticSource::Observed(observation(
        ENDPOINT,
        vec![zero],
    )));
    assert_eq!(snapshot.sources()[0].process_id, None);
    assert_eq!(snapshot.counters().sessions_process_id_zero, 1);
}

// ---------------------------------------------------------------------------
// Process attribution
// ---------------------------------------------------------------------------

/// A readable executable path becomes `win-exe:<full path>`, and the label comes from the
/// OS display name.
#[test]
fn a_readable_executable_path_is_the_only_durable_identifier() {
    let mut attributed = session("attributed");
    attributed.display_name = Some(wide("Synthetic App"));
    let snapshot = run_snapshot(&SyntheticSource::Observed(observation(
        ENDPOINT,
        vec![attributed],
    )));
    let source = &snapshot.sources()[0];
    assert_eq!(source.display_name, "Synthetic App");
    assert_eq!(
        source.app_identifier.as_deref(),
        Some(r"win-exe:C:\Program Files\Synthetic\app.exe")
    );
    assert_eq!(source.process_id, Some(1000));
    assert_eq!(source.kind, AudioSourceKind::Application);
    assert_eq!(source.route, AudioRoute::Ignore);
    assert_eq!(snapshot.counters().attributed_sources, 1);
}

/// With no OS name, the label falls back to the executable basename -- real OS evidence --
/// and never to an invented application name.
#[test]
fn a_missing_label_uses_the_executable_basename() {
    let mut unnamed = session("unnamed");
    unnamed.display_name = None;
    let snapshot = run_snapshot(&SyntheticSource::Observed(observation(
        ENDPOINT,
        vec![unnamed],
    )));
    assert_eq!(snapshot.sources()[0].display_name, "app.exe");
    assert_eq!(snapshot.counters().sources_labelled_from_executable, 1);

    let mut empty_name = session("empty-name");
    empty_name.display_name = Some(wide("   "));
    let snapshot = run_snapshot(&SyntheticSource::Observed(observation(
        ENDPOINT,
        vec![empty_name],
    )));
    assert_eq!(snapshot.sources()[0].display_name, "app.exe");
}

/// With neither an OS name nor an executable path, the literal unnamed-session label is
/// used and `app_identifier` stays `None`.
#[test]
fn a_missing_label_and_path_uses_the_unnamed_session_label() {
    let mut unlabelled = session("unlabelled");
    unlabelled.display_name = None;
    unlabelled.executable_path = None;
    unlabelled.executable_availability = ApplicationExecutableAvailability::ProcessUnavailable;
    let snapshot = run_snapshot(&SyntheticSource::Observed(observation(
        ENDPOINT,
        vec![unlabelled],
    )));
    let source = &snapshot.sources()[0];
    assert_eq!(source.display_name, "unnamed session");
    assert_eq!(source.app_identifier, None);
    assert_eq!(snapshot.counters().sources_labelled_unnamed, 1);
    assert_eq!(snapshot.counters().unattributed_sources, 1);
}

/// A denied `OpenProcess` is a recorded reason and an unattributed source, not a failed
/// discovery and not an invented path. The session itself is retained.
#[test]
fn denied_process_access_is_diagnosed_and_does_not_fail_discovery() {
    let mut denied = session("denied");
    denied.display_name = Some(wide("Denied App"));
    denied.executable_path = None;
    denied.executable_availability = ApplicationExecutableAvailability::ProcessUnavailable;
    denied.process_creation_time = None;
    denied.metadata_failures = vec![MetadataFailure {
        operation: "OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)".to_string(),
        detail: "opening process 1000 failed: this process has no access to the owner process"
            .to_string(),
        hresult: None,
        win32_code: Some(5),
    }];
    let snapshot = run_snapshot(&SyntheticSource::Observed(observation(
        ENDPOINT,
        vec![denied, session("readable")],
    )));
    assert_eq!(
        snapshot.sources().len(),
        2,
        "unavailable optional metadata must not remove a session"
    );
    let denied_source = snapshot
        .sources()
        .iter()
        .find(|source| source.display_name == "Denied App")
        .expect("the denied session is retained");
    assert_eq!(denied_source.app_identifier, None);
    assert!(denied_source.is_active);
    assert_eq!(snapshot.counters().metadata_reads_failed, 1);

    let record = &snapshot.endpoints()[0].sessions[0];
    assert!(record.read_failure.is_none(), "no essential read failed");
    assert!(record.skip_reason.is_none(), "the session is not skipped");
    assert_eq!(record.metadata_failures.len(), 1);
    let diagnostics = render_diagnostics(&snapshot);
    assert!(diagnostics.contains("no access to the owner process"));
    assert!(
        diagnostics.contains("optional metadata unavailable"),
        "{diagnostics}"
    );
    assert!(diagnostics.contains("the session is kept"));
}

/// A session the OS reports as ended is diagnosed and excluded, and unrelated sessions
/// are unaffected.
#[test]
fn a_vanished_session_is_diagnosed() {
    let mut vanished = session("vanished");
    vanished.session_identifier = WideString::new(String::new(), 0);
    vanished.instance_identifier = WideString::new(String::new(), 0);
    vanished.essential_failure = Some(EssentialSessionFailure {
        operation: "IAudioSessionControl2::GetSessionIdentifier".to_string(),
        kind: SessionFailureKind::SessionIdentifier,
        detail: "the session ended during the pass".to_string(),
        hresult: Some(0x8889_0004_u32 as i32),
        win32_code: None,
        recoverable_disappearance: true,
    });
    let snapshot = run_snapshot(&SyntheticSource::Observed(observation(
        ENDPOINT,
        vec![vanished, session("survivor")],
    )));
    assert_eq!(snapshot.sources().len(), 1);
    assert_eq!(snapshot.counters().session_reads_failed, 1);
    let diagnostics = render_diagnostics(&snapshot);
    assert!(diagnostics.contains("0x88890004"), "{diagnostics}");
}

/// An empty session identity cannot be turned into an identity, so the record is skipped
/// and explained.
#[test]
fn an_empty_identity_is_skipped_with_a_reason() {
    let mut empty_instance = session("empty-instance");
    empty_instance.instance_identifier = WideString::new(String::new(), 0);
    let mut empty_session = session_with_identity("", "instance");
    empty_session.display_name = Some(wide("Empty session id"));
    let snapshot = run_snapshot(&SyntheticSource::Observed(observation(
        ENDPOINT,
        vec![empty_instance, empty_session, session("fine")],
    )));
    assert_eq!(snapshot.sources().len(), 1);
    assert_eq!(snapshot.counters().sessions_missing_identity, 2);
    assert_eq!(
        snapshot.endpoints()[0].sessions[0].skip_reason,
        Some(ApplicationSessionSkipReason::MissingSessionIdentity {
            missing: "session instance identifier"
        })
    );
    assert_eq!(
        snapshot.endpoints()[0].sessions[1].skip_reason,
        Some(ApplicationSessionSkipReason::MissingSessionIdentity {
            missing: "session identifier"
        })
    );
}

// ---------------------------------------------------------------------------
// Endpoints
// ---------------------------------------------------------------------------

/// A real empty session list is a valid success.
#[test]
fn an_empty_session_list_is_a_valid_snapshot() {
    let snapshot = run_snapshot(&SyntheticSource::Observed(observation(
        ENDPOINT,
        Vec::new(),
    )));
    assert!(snapshot.sources().is_empty());
    assert_eq!(snapshot.counters().sessions_total, 0);
    assert_eq!(snapshot.counters().endpoints_reported, 1);
    assert_eq!(snapshot.counters().endpoints_retained, 1);
}

/// A pass with no endpoints at all is also a valid, explained empty snapshot.
#[test]
fn a_pass_with_no_endpoints_is_valid_and_explained() {
    let snapshot = run_snapshot(&SyntheticSource::Observed(header(Vec::new())));
    assert!(snapshot.sources().is_empty());
    assert_eq!(snapshot.counters().endpoints_reported, 0);
    let diagnostics = render_diagnostics(&snapshot);
    assert!(diagnostics.contains("A real empty list is a valid result"));
}

/// A read that stopped an endpoint is a snapshot failure, even though the other endpoint
/// was read successfully. A failed pass must never look like a shorter successful one.
#[test]
fn a_failed_endpoint_fails_the_whole_snapshot() {
    let error = snapshot_error(&SyntheticSource::Observed(failed_header(
        vec![
            endpoint("endpoint-broken", Vec::new()),
            endpoint("endpoint-fine", vec![session("kept")]),
        ],
        vec![FailureEvent::from_win32(
            "IAudioSessionManager2::GetSessionEnumerator",
            "the session enumerator could not be obtained".to_string(),
            5,
        )],
    )));
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(
        error.message.contains("GetSessionEnumerator"),
        "{}",
        error.message
    );
    assert!(error.message.contains("Win32 error 5"), "{}", error.message);
}

/// An endpoint that stopped being active is recorded with its observed state and counted;
/// its sessions are not treated as application sources.
#[test]
fn an_inactive_endpoint_is_recorded_not_silently_empty() {
    let mut inactive = endpoint("endpoint-inactive", vec![session("ignored")]);
    inactive.device_state = 8;
    let snapshot = run_snapshot(&SyntheticSource::Observed(header(vec![inactive])));
    assert!(snapshot.sources().is_empty());
    assert_eq!(snapshot.counters().sessions_total, 0);
    assert_eq!(snapshot.counters().endpoints_inactive, 1);
    match snapshot.endpoints()[0].issue.as_ref() {
        Some(ApplicationEndpointIssue::InactiveDeviceState { state, .. }) => assert_eq!(*state, 8),
        other => panic!("expected an inactive device-state issue, got {other:?}"),
    }
}

/// A reported endpoint count past the bound fails the snapshot; the mapper never accepts
/// a count it cannot process.
#[test]
fn a_reported_endpoint_count_past_the_bound_fails_the_snapshot() {
    let oversized = ObservationHeader::fixture(Vec::new(), MAX_ENDPOINTS_PER_SNAPSHOT + 5);
    let error = snapshot_error(&SyntheticSource::Observed(oversized));
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(error.message.contains("128"), "{}", error.message);
}

/// Endpoints beyond the bound are refused by the mapper, never reported as unprocessed.
#[test]
fn endpoints_beyond_the_bound_fail_rather_than_truncate() {
    let endpoints: Vec<ObservedEndpoint> = (0..=MAX_ENDPOINTS_PER_SNAPSHOT)
        .map(|index| endpoint(&format!("endpoint-{index}"), Vec::new()))
        .collect();
    let reported = endpoints.len();
    let error = snapshot_error(&SyntheticSource::Observed(ObservationHeader::fixture(
        endpoints, reported,
    )));
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(error.message.contains("endpoint"), "{}", error.message);
}

/// An endpoint observation without an identity is refused; no endpoint identity is
/// invented for it.
#[test]
fn an_endpoint_without_an_identity_is_refused() {
    let mut anonymous = endpoint("ignored", Vec::new());
    anonymous.endpoint_id = String::new();
    let error = snapshot_error(&SyntheticSource::Observed(header(vec![anonymous])));
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(error.message.contains("endpoint"), "{}", error.message);
}

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// One retained native string past the per-string bound fails the snapshot explicitly
/// instead of being truncated.
#[test]
fn an_oversized_native_string_fails_the_snapshot() {
    let mut oversized = session("oversized");
    oversized.display_name = Some(wide_with_units("Synthetic", MAX_NATIVE_STRING_UTF16 + 1));
    let error = snapshot_error(&SyntheticSource::Observed(observation(
        ENDPOINT,
        vec![oversized],
    )));
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(error.message.contains("UTF-16"), "{}", error.message);
    assert!(
        error
            .message
            .contains("refused rather than silently truncated"),
        "{}",
        error.message
    );

    // Exactly at the bound is accepted.
    let mut at_bound = session("at-bound");
    at_bound.display_name = Some(wide_with_units("Synthetic", MAX_NATIVE_STRING_UTF16));
    let snapshot = run_snapshot(&SyntheticSource::Observed(observation(
        ENDPOINT,
        vec![at_bound],
    )));
    assert_eq!(snapshot.sources().len(), 1);
}

/// A recorded collection failure fails the snapshot with its API name and native code.
#[test]
fn a_recorded_collection_failure_fails_the_snapshot() {
    let error = snapshot_error(&SyntheticSource::Observed(failed_header(
        Vec::new(),
        vec![FailureEvent::from_hresult(
            "IMMDeviceEnumerator::EnumAudioEndpoints",
            "the endpoint enumeration failed",
            0x8000_4005_u32 as i32,
        )],
    )));
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(
        error.message.contains("EnumAudioEndpoints"),
        "{}",
        error.message
    );
    assert!(error.message.contains("80004005"), "{}", error.message);
}

/// More sessions than the snapshot bound fails the snapshot explicitly.
#[test]
fn more_sessions_than_the_bound_fail_the_snapshot() {
    let sessions: Vec<ObservedSession> = (0..=MAX_SESSIONS_PER_SNAPSHOT)
        .map(|index| session(&format!("instance-{index}")))
        .collect();
    let error = snapshot_error(&SyntheticSource::Observed(observation(ENDPOINT, sessions)));
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(
        error.message.contains("session-discovery bound"),
        "{}",
        error.message
    );

    // Exactly at the bound is accepted.
    let sessions: Vec<ObservedSession> = (0..MAX_SESSIONS_PER_SNAPSHOT)
        .map(|index| session(&format!("instance-{index}")))
        .collect();
    let snapshot = run_snapshot(&SyntheticSource::Observed(observation(ENDPOINT, sessions)));
    assert_eq!(snapshot.sources().len(), MAX_SESSIONS_PER_SNAPSHOT);
    assert!(snapshot.counters().payload_bytes < MAX_SNAPSHOT_PAYLOAD_BYTES);
}

/// More endpoints than the snapshot bound fails the snapshot explicitly.
#[test]
fn more_endpoints_than_the_bound_fail_the_snapshot() {
    let endpoints: Vec<ObservedEndpoint> = (0..=MAX_ENDPOINTS_PER_SNAPSHOT)
        .map(|index| endpoint(&format!("endpoint-{index}"), Vec::new()))
        .collect();
    let error = snapshot_error(&SyntheticSource::Observed(header(endpoints)));
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(error.message.contains("endpoint"), "{}", error.message);
}

/// The payload ledger charges checked byte counts and refuses to overflow.
#[test]
fn the_payload_ledger_is_checked() {
    let mut budget = SnapshotBudget::new();
    budget
        .charge_bytes(1024, "a test charge")
        .expect("a charge");
    assert_eq!(budget.charged_bytes(), 1024);
    let error = budget
        .charge_bytes(MAX_SNAPSHOT_PAYLOAD_BYTES, "an oversized charge")
        .expect_err("the payload bound must fail");
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(
        error.message.contains("session-discovery bound"),
        "{}",
        error.message
    );
    assert_eq!(
        budget.charged_bytes(),
        1024,
        "a refused charge is not applied"
    );

    let mut overflow = SnapshotBudget::new();
    overflow
        .charge_bytes(1024, "a first charge")
        .expect("a first charge");
    // 1024 + `usize::MAX` cannot be represented, so the checked addition itself must
    // catch it instead of wrapping around to a small total.
    let error = overflow
        .charge_bytes(usize::MAX, "a charge that cannot be added")
        .expect_err("checked addition overflow must fail");
    assert!(error.message.contains("overflowed"), "{}", error.message);
    assert_eq!(
        overflow.charged_bytes(),
        1024,
        "a refused charge is not applied"
    );
}

/// The endpoint and session ledgers refuse to exceed their caps.
#[test]
fn the_count_ledgers_refuse_to_exceed_their_caps() {
    let mut budget = SnapshotBudget::new();
    for index in 0..MAX_ENDPOINTS_PER_SNAPSHOT {
        budget.charge_endpoint(index).expect("within the bound");
    }
    assert_eq!(budget.endpoints(), MAX_ENDPOINTS_PER_SNAPSHOT);
    assert!(budget.charge_endpoint(MAX_ENDPOINTS_PER_SNAPSHOT).is_err());

    let mut budget = SnapshotBudget::new();
    for index in 0..MAX_SESSIONS_PER_SNAPSHOT {
        budget.charge_session(index).expect("within the bound");
    }
    assert_eq!(budget.sessions(), MAX_SESSIONS_PER_SNAPSHOT);
    assert!(budget.charge_session(MAX_SESSIONS_PER_SNAPSHOT).is_err());
}

/// The published limits are the enforced constants.
#[test]
fn the_published_limits_are_the_enforced_bounds() {
    assert_eq!(
        APPLICATION_SESSION_LIMITS.max_endpoints_per_snapshot,
        MAX_ENDPOINTS_PER_SNAPSHOT
    );
    assert_eq!(
        APPLICATION_SESSION_LIMITS.max_sessions_per_snapshot,
        MAX_SESSIONS_PER_SNAPSHOT
    );
    assert_eq!(
        APPLICATION_SESSION_LIMITS.max_native_string_utf16,
        MAX_NATIVE_STRING_UTF16
    );
    assert_eq!(
        APPLICATION_SESSION_LIMITS.max_snapshot_payload_bytes,
        MAX_SNAPSHOT_PAYLOAD_BYTES
    );
    let snapshot = run_snapshot(&SyntheticSource::Observed(observation(
        ENDPOINT,
        Vec::new(),
    )));
    assert_eq!(snapshot.limits(), APPLICATION_SESSION_LIMITS);
}

// ---------------------------------------------------------------------------
// Worker boundary
// ---------------------------------------------------------------------------

/// The observation runs on a different thread from the caller, and each call takes a
/// fresh worker.
#[test]
fn each_call_runs_one_fresh_worker_thread() {
    let count = Arc::new(AtomicUsize::new(0));
    let source = SyntheticSource::Counting(
        Arc::clone(&count),
        observation(ENDPOINT, vec![session("instance")]),
    );
    let caller = thread_id();
    let first = snapshot_from(&source).expect("first pass");
    let second = snapshot_from(&source).expect("second pass");
    assert_eq!(count.load(Ordering::SeqCst), 2, "one observation per call");
    assert_eq!(first.sources().len(), 1);
    assert_eq!(second.sources().len(), 1);
    assert_eq!(ids(&first), ids(&second));
    assert!(caller != 0);
}

/// A pass that cannot be taken is an explicit error, never an empty success.
#[test]
fn a_failed_pass_is_not_an_empty_success() {
    let error = snapshot_error(&SyntheticSource::Failed(RecorderError {
        code: ErrorCode::Internal,
        message: "EnumAudioEndpoints failed".to_string(),
        recovery: RecoveryStatus::NoRecording,
        action: "retry".to_string(),
    }));
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(error.message.contains("EnumAudioEndpoints"));
}

/// A panicking worker becomes an explicit `Internal` error carrying the panic text, and
/// never an empty snapshot.
#[test]
fn a_panicking_worker_propagates_as_an_error() {
    let error = snapshot_error(&SyntheticSource::Panics);
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(error.message.contains("panicked"), "{}", error.message);
    assert!(
        error
            .message
            .contains("synthetic session observation panicked"),
        "{}",
        error.message
    );
    assert_eq!(error.recovery, RecoveryStatus::NoRecording);
}

/// A worker panic does not poison the next call: the boundary is per call.
#[test]
fn a_worker_panic_does_not_affect_the_next_call() {
    assert!(snapshot_from(&SyntheticSource::Panics).is_err());
    let snapshot = run_snapshot(&SyntheticSource::Observed(observation(
        ENDPOINT,
        vec![session("after-panic")],
    )));
    assert_eq!(snapshot.sources().len(), 1);
}

/// The calling thread's identity is unchanged and no caller thread state is required: the
/// worker is joined before the call returns, so repeated calls are ordered.
#[test]
fn calls_are_sequential_and_joined() {
    let count = Arc::new(AtomicUsize::new(0));
    let source = SyntheticSource::Counting(
        Arc::clone(&count),
        observation(ENDPOINT, vec![session("instance")]),
    );
    for _ in 0..4 {
        snapshot_from(&source).expect("pass");
    }
    assert_eq!(count.load(Ordering::SeqCst), 4);
}

/// The caller's thread ID, used to show the observation ran elsewhere.
fn thread_id() -> u64 {
    let handle = std::thread::current();
    let text = format!("{:?}", handle.id());
    text.trim_start_matches("ThreadId(")
        .trim_end_matches(')')
        .parse()
        .unwrap_or(0)
}

/// Strips the terminator of the last identity component.
trait ExpectTerminator {
    /// The component payload without its trailing colon.
    fn expect_terminator(self) -> String;
}

impl ExpectTerminator for &str {
    fn expect_terminator(self) -> String {
        self.strip_suffix(':')
            .unwrap_or_else(|| panic!("the last identity component must be terminated: {self:?}"))
            .to_string()
    }
}

// ---------------------------------------------------------------------------
// Diagnostics report
// ---------------------------------------------------------------------------

/// The report states what it is and is not, and explains every class of excluded record.
#[test]
fn the_report_is_honest_about_its_limits() {
    let mut system = session("system");
    system.is_system_sounds = true;
    let mut expired = session("expired");
    expired.raw_state = 2;
    let mut multiprocess = session("multiprocess");
    multiprocess.process_id = None;
    multiprocess.process_availability = ApplicationProcessAvailability::MultiProcessSession;
    multiprocess.executable_path = None;
    multiprocess.executable_availability = ApplicationExecutableAvailability::NoSingleProcess;
    let mut unnamed = session("unnamed");
    unnamed.display_name = None;
    unnamed.executable_path = None;
    let mut unavailable = session("unavailable");
    unavailable.display_name = Some(wide("Unavailable App"));
    unavailable.executable_path = None;
    unavailable.executable_availability = ApplicationExecutableAvailability::ProcessUnavailable;
    unavailable.process_creation_time = None;
    unavailable.metadata_failures = vec![MetadataFailure {
        operation: "OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)".to_string(),
        detail: "opening process 1000 failed: the owner process no longer exists".to_string(),
        hresult: None,
        win32_code: Some(1168),
    }];
    let observed = observation(
        ENDPOINT,
        vec![
            system,
            expired,
            multiprocess,
            unnamed,
            unavailable,
            session("kept"),
        ],
    );
    let snapshot = run_snapshot(&SyntheticSource::Observed(observed));
    let diagnostics = render_diagnostics(&snapshot);

    for expected in [
        "read-only diagnostic snapshot",
        "captures no audio",
        "does NOT",
        "FRAME-0106B",
        "not atomic",
        "not a live tracker",
        "A source ID is a live-session selection identity",
        "is_active is the OS session state",
        "AudioSessionStateActive",
        "is not microphone",
        "not process-loopback availability",
        "exceeding any of these fails the snapshot, it never truncates",
        "sessions system sounds (excluded)",
        "sessions expired (excluded)",
        "sessions multiprocess (no single pid)",
        "optional metadata reads failed (source retained)",
        "process creation times observed (private)",
        "sources unattributed",
        "S_OK",
        "skipped [systemSoundsSession]",
        "skipped [expiredSession]",
        "executable path: noSingleProcess",
        "unnamed session",
        "optional metadata unavailable",
        "the session is kept",
        "Bounds (exceeding any of these fails the snapshot, it never truncates)",
    ] {
        assert!(
            diagnostics.contains(expected),
            "the diagnostics report must contain {expected:?}\n{diagnostics}"
        );
    }
    // A retained source is identified as retained, so an empty result cannot be confused
    // with a filtered one.
    assert!(
        diagnostics.contains("retained as a source"),
        "{diagnostics}"
    );
    assert!(
        diagnostics.contains("sessions process id zero"),
        "{diagnostics}"
    );
    // The report must not claim a truncating bound anywhere.
    assert!(!diagnostics.contains("unprocessed"), "{diagnostics}");
}

/// The report shows the multi-process diagnostic reason and does not claim a pid.
#[test]
fn the_report_does_not_claim_a_multiprocess_pid() {
    let mut multiprocess = session("multiprocess");
    multiprocess.process_id = None;
    multiprocess.process_availability = ApplicationProcessAvailability::MultiProcessSession;
    multiprocess.executable_path = None;
    multiprocess.executable_availability = ApplicationExecutableAvailability::NoSingleProcess;
    multiprocess.display_name = Some(wide("Multi App"));
    let snapshot = run_snapshot(&SyntheticSource::Observed(observation(
        ENDPOINT,
        vec![multiprocess],
    )));
    let diagnostics = render_diagnostics(&snapshot);
    assert!(diagnostics.contains("Multi App"));
    assert!(diagnostics.contains("noSingleProcess"), "{diagnostics}");
    assert!(
        diagnostics.contains("more than one process"),
        "{diagnostics}"
    );
    assert!(
        diagnostics.contains("process id: none (no single process"),
        "{diagnostics}"
    );
}

/// The canonical snapshot accessors agree with the report and with each other.
#[test]
fn the_snapshot_accessors_agree() {
    let snapshot = run_snapshot(&SyntheticSource::Observed(observation(
        ENDPOINT,
        vec![session("one"), session("two")],
    )));
    assert_eq!(snapshot.sources().len(), 2);
    assert_eq!(snapshot.endpoints().len(), 1);
    assert_eq!(snapshot.counters().sessions_total, 2);
    assert_eq!(snapshot.counters().endpoints_retained, 1);
    assert_eq!(snapshot.limits(), APPLICATION_SESSION_LIMITS);
    for source in snapshot.sources() {
        assert_eq!(source.kind, AudioSourceKind::Application);
        assert_eq!(source.route, AudioRoute::Ignore);
        assert!(source.is_active);
        assert!(source.id.0.starts_with("wasapi-session:"));
        assert_eq!(
            decode_application_session_id(&source.id.0).map(|(endpoint, _)| endpoint),
            Some(ENDPOINT.to_string())
        );
    }
    let consumed = snapshot.into_sources();
    assert_eq!(consumed.len(), 2);
}

fn review_error(source: &SyntheticSource) -> RecorderError {
    match snapshot_from(source) {
        Err(error) => error,
        Ok(snapshot) => panic!(
            "ADR 0007 requires failure; got successful snapshot with {} sources",
            snapshot.sources().len()
        ),
    }
}
// Integration regressions. These assert ADR 0007 behavior through the production
// mapper and the production native collector seam rather than through successful fixtures.
#[test]
fn integration_review_endpoint_enumeration_failure_is_not_empty_success() {
    // The production reader records a failed endpoint enumeration as a collection failure
    // with the API name and native code; the mapper must turn it into an error instead of
    // returning an empty Vec.
    let error = review_error(&SyntheticSource::Observed(failed_header(
        vec![endpoint(ENDPOINT, Vec::new())],
        vec![FailureEvent::from_hresult(
            "IAudioSessionManager2::GetSessionEnumerator",
            "the session enumerator could not be obtained",
            0x8000_4005_u32 as i32,
        )],
    )));
    assert!(error.message.contains("GetSessionEnumerator"));
    assert!(error.message.contains("80004005"));
}

#[test]
fn integration_review_truncated_endpoints_fail_the_snapshot() {
    // A collection that reports more endpoints than the bound observed fewer of them.
    let observed = ObservationHeader::fixture(
        (0..MAX_ENDPOINTS_PER_SNAPSHOT)
            .map(|index| endpoint(&format!("endpoint-{index}"), Vec::new()))
            .collect(),
        MAX_ENDPOINTS_PER_SNAPSHOT + 1,
    );
    review_error(&SyntheticSource::Observed(observed));
}

#[test]
fn integration_review_truncated_sessions_fail_the_snapshot() {
    // The production collector refuses an endpoint whose reported session count exceeds
    // the remaining snapshot allowance before it reads or allocates any session; the
    // mapper also refuses a session list past the bound given directly.
    review_error(&SyntheticSource::Observed(ObservationHeader::fixture(
        vec![endpoint(
            ENDPOINT,
            (0..=MAX_SESSIONS_PER_SNAPSHOT)
                .map(|index| session(&format!("instance-{index}")))
                .collect(),
        )],
        1,
    )));
}

#[test]
fn integration_review_endpoint_name_obeys_the_string_bound() {
    let mut oversized = endpoint(ENDPOINT, Vec::new());
    oversized.friendly_name = Some(wide_with_units("x", MAX_NATIVE_STRING_UTF16 + 1));
    review_error(&SyntheticSource::Observed(header(vec![oversized])));
}

#[test]
fn integration_review_endpoint_identity_bound_counts_utf16_units() {
    // A supplementary-plane identity is two UTF-16 units per scalar, so this is
    // `MAX_NATIVE_STRING_UTF16 + 2` units and must be refused even though its scalar count
    // is only `MAX_NATIVE_STRING_UTF16 / 2 + 1`.
    let oversized = "\u{1f600}".repeat(MAX_NATIVE_STRING_UTF16 / 2 + 1);
    let units = oversized.encode_utf16().count();
    assert!(units > MAX_NATIVE_STRING_UTF16);
    let mut observed = endpoint(ENDPOINT, Vec::new());
    observed.endpoint_id = oversized;
    review_error(&SyntheticSource::Observed(header(vec![observed])));
}

#[test]
fn integration_review_denied_process_metadata_keeps_the_session() {
    // This is the observation the production reader emits for an `OpenProcess` failure:
    // an optional metadata failure beside a session whose identity and state were read.
    let mut denied = session("denied-process");
    denied.executable_path = None;
    denied.executable_availability = ApplicationExecutableAvailability::ProcessUnavailable;
    denied.process_creation_time = None;
    denied.metadata_failures = vec![MetadataFailure {
        operation: "OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)".to_string(),
        detail: "opening process 1000 failed: this process has no access to the owner process"
            .to_string(),
        hresult: None,
        win32_code: Some(5),
    }];
    let snapshot = run_snapshot(&SyntheticSource::Observed(observation(
        ENDPOINT,
        vec![denied],
    )));
    assert_eq!(
        snapshot.sources().len(),
        1,
        "identity and state remain readable"
    );
    assert!(snapshot.sources()[0].app_identifier.is_none());
    assert!(snapshot.sources()[0].is_active);
    assert_eq!(snapshot.counters().metadata_reads_failed, 1);
    assert!(render_diagnostics(&snapshot).contains("OpenProcess"));
}

/// A creation-time-only failure keeps the path-based identifier.
#[test]
fn integration_review_creation_time_failure_keeps_the_identifier() {
    let mut partial = session("partial-metadata");
    partial.process_creation_time = None;
    partial.metadata_failures = vec![MetadataFailure {
        operation: "GetProcessTimes".to_string(),
        detail: "the process creation time could not be read".to_string(),
        hresult: None,
        win32_code: Some(5),
    }];
    let snapshot = run_snapshot(&SyntheticSource::Observed(observation(
        ENDPOINT,
        vec![partial],
    )));
    assert_eq!(snapshot.sources().len(), 1);
    assert_eq!(
        snapshot.sources()[0].app_identifier.as_deref(),
        Some(r"win-exe:C:\Program Files\Synthetic\app.exe"),
        "a successful path keeps its app_identifier when only the creation time fails"
    );
    assert_eq!(snapshot.counters().process_creation_times_observed, 0);
    assert_eq!(snapshot.counters().metadata_reads_failed, 1);
}

/// A full metadata read records the private creation time without exposing it as identity.
#[test]
fn integration_review_full_process_metadata_is_observed() {
    let snapshot = run_snapshot(&SyntheticSource::Observed(observation(
        ENDPOINT,
        vec![session("full-metadata")],
    )));
    assert_eq!(snapshot.counters().process_creation_times_observed, 1);
    assert_eq!(snapshot.counters().metadata_reads_failed, 0);
    assert_eq!(
        snapshot.sources()[0].app_identifier.as_deref(),
        Some(r"win-exe:C:\Program Files\Synthetic\app.exe")
    );
}

#[test]
fn integration_collected_endpoint_limit_is_not_counted_again_by_mapper() {
    let mut observed = header(
        (0..MAX_ENDPOINTS_PER_SNAPSHOT)
            .map(|index| endpoint(&format!("endpoint-{index}"), Vec::new()))
            .collect(),
    );
    for index in 0..MAX_ENDPOINTS_PER_SNAPSHOT {
        observed.budget.charge_endpoint(index).unwrap();
    }
    let snapshot = run_snapshot(&SyntheticSource::Observed(observed));
    assert_eq!(snapshot.endpoints().len(), MAX_ENDPOINTS_PER_SNAPSHOT);
}

#[test]
fn integration_collected_session_limit_is_not_counted_again_by_mapper() {
    let mut observed = observation(
        ENDPOINT,
        (0..MAX_SESSIONS_PER_SNAPSHOT)
            .map(|index| session(&format!("session-{index}")))
            .collect(),
    );
    observed.budget.charge_endpoint(0).unwrap();
    for index in 0..MAX_SESSIONS_PER_SNAPSHOT {
        observed.budget.charge_session(index).unwrap();
    }
    let snapshot = run_snapshot(&SyntheticSource::Observed(observed));
    assert_eq!(snapshot.sources().len(), MAX_SESSIONS_PER_SNAPSHOT);
}

#[test]
fn integration_optional_metadata_preserves_its_win32_code_in_diagnostics() {
    let mut denied = session("denied");
    denied.executable_path = None;
    denied.metadata_failures.push(MetadataFailure {
        operation: "OpenProcess".into(),
        detail: "access denied".into(),
        hresult: None,
        win32_code: Some(5),
    });
    let snapshot = run_snapshot(&SyntheticSource::Observed(observation(
        ENDPOINT,
        vec![denied],
    )));
    assert_eq!(snapshot.sources().len(), 1);
    assert!(render_diagnostics(&snapshot).contains("Win32 error 5"));
}
