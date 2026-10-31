// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! The application-session worker boundary (ADR 0007 FRAME-0106A).
//!
//! Every call enumerates on **one fresh, joined** MTA worker thread. The caller's COM
//! apartment is not entered and not changed; the worker initializes and uninitializes
//! MTA on itself. Only plain owned values cross the boundary in either direction: no
//! COM interface, buffer, event or handle is ever handed to the caller, and the worker
//! has fully unwound before the call returns.
//!
//! The boundary is also the failure boundary. A worker that panics - including a panic
//! inside the reviewed native module - is turned into an explicit `Internal` error with
//! the panic text, never into an empty successful snapshot. The unwind happens on the
//! owner thread, so the apartment guard and every COM reference are released there
//! before the join completes.
//!
//! A returned observation is either complete for its pass or an error. There is no
//! "successful but incomplete" shape: a run that had to stop early records a
//! [`FailureEvent`], and the mapper turns every one of those into an explicit error
//! instead of reporting a shortened source list.
//!
//! The whole boundary exists for the Windows observer: on a non-Windows build nothing
//! calls it, so dead-code analysis is silenced for the module there instead of keeping a
//! stub worker alive.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use std::any::Any;
use std::sync::mpsc;
use std::thread;

use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus};

use super::limits::SnapshotBudget;

/// One retained native string as the OS reported it, before UTF-8 conversion.
///
/// The UTF-16 content length is kept so the per-string bound is checked in the units the
/// ADR names, separately from the terminator.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WideString {
    text: String,
    units: usize,
}

impl WideString {
    /// Retains already-converted text with its UTF-16 content length.
    pub(crate) fn new(text: String, units: usize) -> Self {
        Self { text, units }
    }

    /// Retains one OS string from its UTF-16 code units.
    ///
    /// Returns `None` when the code units are not valid UTF-16: identity and path data
    /// must convert exactly, because a replacement character could make two different
    /// sessions or processes collide on one identity. The caller records that explicitly
    /// instead of inventing a value.
    #[cfg(test)]
    pub(crate) fn try_from_utf16(units: &[u16]) -> Option<Self> {
        let text = String::from_utf16(units).ok()?;
        Some(Self {
            text,
            units: units.len(),
        })
    }

    /// The retained UTF-16 content unit count, excluding any terminator.
    pub(crate) fn units(&self) -> usize {
        self.units
    }

    /// Takes the owned text, without re-allocating it.
    pub(crate) fn into_string(self) -> String {
        self.text
    }

    /// Borrows the text.
    pub(crate) fn as_str(&self) -> &str {
        &self.text
    }
}

/// Which essential session read failed.
///
/// Essential reads establish identity, state or process ownership. A failure here is
/// either a specifically recognized recoverable disappearance or a snapshot failure,
/// and it is never silently converted into a plausible-looking default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionFailureKind {
    /// `IAudioSessionControl::GetState` failed.
    State,
    /// `IAudioSessionControl2::GetSessionIdentifier` failed.
    SessionIdentifier,
    /// `IAudioSessionControl2::GetSessionInstanceIdentifier` failed.
    InstanceIdentifier,
    /// `IAudioSessionControl2::IsSystemSoundsSession` failed.
    SystemSounds,
    /// `IAudioSessionControl2::GetProcessId` failed.
    ProcessId,
}

impl SessionFailureKind {
    /// The API-shaped name of the failing read.
    pub(crate) fn operation(self) -> &'static str {
        match self {
            Self::State => "IAudioSessionControl::GetState",
            Self::SessionIdentifier => "IAudioSessionControl2::GetSessionIdentifier",
            Self::InstanceIdentifier => "IAudioSessionControl2::GetSessionInstanceIdentifier",
            Self::SystemSounds => "IAudioSessionControl2::IsSystemSoundsSession",
            Self::ProcessId => "IAudioSessionControl2::GetProcessId",
        }
    }
}

/// One essential session read that failed, with the code the platform reported.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EssentialSessionFailure {
    pub(crate) kind: SessionFailureKind,
    pub(crate) operation: String,
    pub(crate) detail: String,
    pub(crate) hresult: Option<i32>,
    pub(crate) win32_code: Option<u32>,
    /// True only when the code is one this module documents as "the session is gone",
    /// so the record may be skipped with a reason instead of failing the snapshot.
    pub(crate) recoverable_disappearance: bool,
}

/// One optional process-metadata read that failed.
///
/// Optional metadata never removes an otherwise valid source: the session keeps its
/// identity, activity and route, and `app_identifier` stays `None` with this reason.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MetadataFailure {
    pub(crate) operation: String,
    pub(crate) detail: String,
    pub(crate) hresult: Option<i32>,
    pub(crate) win32_code: Option<u32>,
}

/// One session exactly as this pass read it.
///
/// `essential_failure` decides whether the session can be reported at all;
/// `metadata_failures` only explain why optional attribution is absent.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ObservedSession {
    pub(crate) session_identifier: WideString,
    pub(crate) instance_identifier: WideString,
    pub(crate) raw_state: i32,
    pub(crate) display_name: Option<WideString>,
    pub(crate) is_system_sounds: bool,
    pub(crate) process_id: Option<u32>,
    pub(crate) process_availability: super::ApplicationProcessAvailability,
    pub(crate) executable_path: Option<WideString>,
    /// Which executable-path attribute the session has.
    pub(crate) executable_availability: super::ApplicationExecutableAvailability,
    /// The process creation time observed through the same process handle as the image
    /// path, as a private lifetime-evidence value. Never a shared contract field and
    /// never a PID-reuse guarantee.
    pub(crate) process_creation_time: Option<u64>,
    /// The essential read that failed, when one did.
    pub(crate) essential_failure: Option<EssentialSessionFailure>,
    /// Optional metadata reads that failed. These never exclude the session.
    pub(crate) metadata_failures: Vec<MetadataFailure>,
}

/// One endpoint exactly as this pass read it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ObservedEndpoint {
    /// The endpoint's own identity, read once from `IMMDevice::GetId`.
    pub(crate) endpoint_id: String,
    /// OS device friendly name, when it has one.
    pub(crate) friendly_name: Option<WideString>,
    /// Raw `DEVICE_STATE` bits read for this endpoint in this pass.
    pub(crate) device_state: u32,
    pub(crate) sessions: Vec<ObservedSession>,
}

/// One read that stopped part of the pass, recorded as plain values.
///
/// Every failure event fails the snapshot in the mapper; carrying them as values keeps the
/// mapper pure and testable without duplicating the native error text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FailureEvent {
    pub(crate) operation: String,
    pub(crate) detail: String,
    pub(crate) hresult: Option<i32>,
    pub(crate) win32_code: Option<u32>,
    /// Invalid native data is a pass failure, even when this read was optional.
    pub(crate) fatal: bool,
}

impl FailureEvent {
    /// A failure event from an operation and its detail.
    pub(crate) fn new(operation: &str, detail: String) -> Self {
        Self {
            operation: operation.to_string(),
            detail,
            hresult: None,
            win32_code: None,
            fatal: false,
        }
    }

    /// A failure event from a platform error code.
    ///
    /// This takes the code rather than the platform's error type, so the event type itself
    /// carries no Windows dependency and the collector converts at the call site.
    pub(crate) fn from_hresult(operation: &str, detail: &str, hresult: i32) -> Self {
        Self {
            operation: operation.to_string(),
            detail: detail.to_string(),
            hresult: Some(hresult),
            win32_code: None,
            fatal: false,
        }
    }

    /// A failure event from a Win32 error code.
    pub(crate) fn from_win32(operation: &str, detail: String, code: u32) -> Self {
        Self {
            operation: operation.to_string(),
            detail,
            hresult: None,
            win32_code: Some(code),
            fatal: false,
        }
    }

    pub(crate) fn invalid(operation: &str, detail: String) -> Self {
        let mut event = Self::new(operation, detail);
        event.fatal = true;
        event
    }
}

/// Everything one observation pass produced, as plain owned values.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ObservationHeader {
    pub(crate) endpoints: Vec<ObservedEndpoint>,
    /// Endpoints Windows reported in this pass.
    pub(crate) endpoints_reported: usize,
    /// Every read that stopped part of the pass. Non-empty means the mapper fails.
    pub(crate) failures: Vec<FailureEvent>,
    /// The cumulative owned-storage ledger the collector charged, carried into the
    /// mapper so one monotonic total covers both stages, with no reset between them.
    pub(crate) budget: SnapshotBudget,
}

impl ObservationHeader {
    /// An observation with no failures, for the synthetic seam.
    ///
    /// The tests compose observations directly through this and [`ObservationHeader::with_failures`];
    /// each one is visibly a fixture rather than a production read.
    #[cfg(test)]
    pub(crate) fn fixture(endpoints: Vec<ObservedEndpoint>, endpoints_reported: usize) -> Self {
        Self {
            endpoints,
            endpoints_reported,
            failures: Vec::new(),
            budget: SnapshotBudget::new(),
        }
    }

    /// The same fixture with explicit collection failures and an explicit ledger.
    #[cfg(test)]
    pub(crate) fn with_failures(
        endpoints: Vec<ObservedEndpoint>,
        endpoints_reported: usize,
        failures: Vec<FailureEvent>,
        budget: SnapshotBudget,
    ) -> Self {
        Self {
            endpoints,
            endpoints_reported,
            failures,
            budget,
        }
    }
}

/// The private seam over the native observation.
///
/// Production implements this with MMDevice and Core Audio; the tests implement it with a
/// visibly synthetic source, so the mapping, identity, ordering and bound rules run on
/// every host without hardware. Nothing here is a second implementation of the production
/// rules: implementations only *report* what they saw.
pub(crate) trait ApplicationObservationSource {
    /// Reads one whole snapshot of endpoints and their sessions.
    ///
    /// An error means the pass could not be taken at all and must never be reported as an
    /// empty snapshot. Recoverable outcomes are recorded inside the returned records;
    /// anything that stopped part of the pass is recorded in `failures`, which the mapper
    /// turns into an error.
    fn observe_application_sessions(&self) -> Result<ObservationHeader, RecorderError>;
}

/// Runs one observation on a fresh joined MTA worker thread.
///
/// `thread::Builder::spawn_scoped` returns an `io::Error` when the thread cannot be
/// created; that is an explicit failure, not an empty snapshot. A worker panic is caught
/// and converted to an `Internal` error carrying the panic text. The result channel is
/// the only way a result crosses back, so a worker that returns without sending is also
/// an explicit failure rather than a hang.
///
/// The scoped thread is joined before this function returns, so the apartment guard and
/// every COM reference the worker owns are released on the worker thread, and nothing
/// native outlives the call.
pub(crate) fn run_joined(
    source: &(impl ApplicationObservationSource + Sync),
) -> Result<ObservationHeader, RecorderError> {
    let (sender, receiver) = mpsc::sync_channel::<Result<ObservationHeader, RecorderError>>(1);
    thread::scope(|scope| {
        let worker = thread::Builder::new()
            .name("frame-app-sessions".to_string())
            .spawn_scoped(scope, move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    source.observe_application_sessions()
                }))
                .unwrap_or_else(|payload| Err(worker_panic_error(&describe_panic(&payload))));
                // Ownership of every COM object and of the apartment guard ends here, on
                // the worker thread, before this closure returns and the join completes.
                let _ = sender.send(outcome);
            })
            .map_err(worker_spawn_error)?;
        // Join first: no worker, apartment or COM object may outlive the call.
        if worker.join().is_err() {
            return Err(worker_panic_error(
                "the application-session worker thread panicked and unwound past its own result",
            ));
        }
        // The worker sent exactly one result or dropped the sender without one.
        receiver.recv().map_err(|_| worker_exit_error())?
    })
}

/// The panic text of one worker panic payload.
fn describe_panic(payload: &Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "the worker panicked with a payload that is neither &str nor String".to_string()
}

/// An observation worker could not be created.
fn worker_spawn_error(error: std::io::Error) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message: format!(
            "The application-session worker thread could not be created: {error}. No snapshot was taken."
        ),
        recovery: RecoveryStatus::NoRecording,
        action: "Retry application-session discovery; if it persists, report the error."
            .to_string(),
    }
}

/// The observation worker panicked.
fn worker_panic_error(detail: &str) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message: format!(
            "The application-session worker thread panicked: {detail}. No snapshot was reported from a panicking worker."
        ),
        recovery: RecoveryStatus::NoRecording,
        action: "Retry application-session discovery; if it persists, report the error and the Windows build."
            .to_string(),
    }
}

/// The observation worker exited without reporting a result.
fn worker_exit_error() -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message: "The application-session worker thread exited without reporting a snapshot, so no result can be claimed."
            .to_string(),
        recovery: RecoveryStatus::NoRecording,
        action: "Retry application-session discovery; if it persists, report the error."
            .to_string(),
    }
}
