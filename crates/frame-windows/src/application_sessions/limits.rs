// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Bound accounting for one application-session snapshot (ADR 0007 FRAME-0106A).
//!
//! The ADR bounds a snapshot to 128 render endpoints, 1024 sessions in total across
//! those endpoints, 4096 UTF-16 units per retained native string and 32 MiB of
//! Frame-owned snapshot storage. This module owns those constants and the single ledger
//! that checks them, so the native collector and the mapper cannot each invent their own
//! arithmetic.
//!
//! Every limit is a **failure** limit, never a truncation limit: exceeding a bound is an
//! explicit `Internal` error and never a silently shortened list. Operating-system and
//! COM-internal allocations are not claimed to be bounded by these numbers.
//!
//! # What the byte ledger covers
//!
//! One monotonically growing charge per snapshot, carried from the collector into the
//! mapper. It charges the owned storage a snapshot carries: the retained strings, the
//! canonical identity and label strings, the record and source vectors, native read
//! scratch, and each
//! retained value's UTF-8 expansion. It does not refund storage that is dropped between
//! the two stages, so the total is an upper bound on the peak rather than an exact
//! measurement, and it is deliberately conservative for that reason.
//!
//! A charge is reserved **before** the storage is created, and it uses
//! `String::with_capacity`/`Vec::with_capacity`. Actual capacities are checked after
//! allocation as well. OS/COM allocations, the process handle table's bookkeeping and the
//! allocator's own metadata are explicitly outside this metric. Serializing the snapshot
//! to text or JSON is a separate phase whose peak is not part of this metric.
//!
//! The ledger exists for the Windows observer: on a non-Windows build nothing charges or
//! reads it, so dead-code analysis is silenced for the module there rather than keeping an
//! unused ledger alive.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use frame_contracts::{ErrorCode, RecorderError, RecoveryStatus};

/// At most this many active render endpoints are processed in one snapshot.
pub const MAX_ENDPOINTS_PER_SNAPSHOT: usize = 128;

/// At most this many sessions in total are processed in one snapshot, counted across
/// every endpoint.
pub const MAX_SESSIONS_PER_SNAPSHOT: usize = 1024;

/// At most this many UTF-16 units in one retained native string.
///
/// This is **content** units: the terminator is not part of the limit.
pub const MAX_NATIVE_STRING_UTF16: usize = 4096;

/// At most this many bytes of Frame-owned snapshot storage: the canonical sources
/// returned to the caller plus the owned diagnostic records that describe them.
pub const MAX_SNAPSHOT_PAYLOAD_BYTES: usize = 32 * 1024 * 1024;

/// The published snapshot limits, so a probe reports the bounds the code enforces
/// instead of restating them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplicationSessionLimits {
    pub max_endpoints_per_snapshot: usize,
    pub max_sessions_per_snapshot: usize,
    pub max_native_string_utf16: usize,
    pub max_snapshot_payload_bytes: usize,
}

/// The one accepted set of snapshot limits.
pub const APPLICATION_SESSION_LIMITS: ApplicationSessionLimits = ApplicationSessionLimits {
    max_endpoints_per_snapshot: MAX_ENDPOINTS_PER_SNAPSHOT,
    max_sessions_per_snapshot: MAX_SESSIONS_PER_SNAPSHOT,
    max_native_string_utf16: MAX_NATIVE_STRING_UTF16,
    max_snapshot_payload_bytes: MAX_SNAPSHOT_PAYLOAD_BYTES,
};

/// The owned-byte and count ledger for one snapshot.
///
/// The collector starts it and charges as it retains values; the mapper continues
/// charging into the same ledger while it builds the canonical sources, so one total
/// covers the whole snapshot's owned storage rather than one stage's fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SnapshotBudget {
    /// The limits this ledger enforces. Production uses [`APPLICATION_SESSION_LIMITS`];
    /// the tests may use a smaller private budget to reach the same paths cheaply.
    limits: ApplicationSessionLimits,
    /// Total charges against `limits.max_snapshot_payload_bytes`.
    charged_bytes: usize,
    /// Endpoints retained so far.
    endpoints: usize,
    /// Sessions retained so far, counted across every endpoint. A session is counted even
    /// when its own reads fail, so a run of failures cannot hide behind the cap.
    sessions: usize,
    /// True when native-owned observation storage has already been charged. The mapper
    /// moves those strings into records and must not charge their capacity again.
    native_owned: bool,
}

impl Default for SnapshotBudget {
    fn default() -> Self {
        Self::new()
    }
}

impl SnapshotBudget {
    /// A ledger enforcing the production limits.
    pub(crate) fn new() -> Self {
        Self::with_limits(APPLICATION_SESSION_LIMITS)
    }

    /// A ledger enforcing explicit limits, used by the tests to reach a bound cheaply
    /// through the same production code path.
    pub(crate) fn with_limits(limits: ApplicationSessionLimits) -> Self {
        Self {
            limits,
            charged_bytes: 0,
            endpoints: 0,
            sessions: 0,
            native_owned: false,
        }
    }

    /// The limits this ledger enforces.
    pub(crate) fn limits(&self) -> ApplicationSessionLimits {
        self.limits
    }

    /// How many bytes of Frame-owned snapshot storage have been charged.
    pub(crate) fn charged_bytes(&self) -> usize {
        self.charged_bytes
    }

    pub(crate) fn native_owned(&self) -> bool {
        self.native_owned
    }

    pub(crate) fn mark_native_owned(&mut self) {
        self.native_owned = true;
    }

    /// How many sessions have been charged.
    pub(crate) fn sessions(&self) -> usize {
        self.sessions
    }

    /// How many endpoints have been charged.
    #[cfg(test)]
    pub(crate) fn endpoints(&self) -> usize {
        self.endpoints
    }

    /// How many more sessions this snapshot may charge.
    pub(crate) fn session_allowance(&self) -> usize {
        self.limits
            .max_sessions_per_snapshot
            .saturating_sub(self.sessions)
    }

    /// Charges one endpoint, failing explicitly past the endpoint cap.
    pub(crate) fn charge_endpoint(&mut self, order: usize) -> Result<(), RecorderError> {
        if self.endpoints >= self.limits.max_endpoints_per_snapshot {
            return Err(endpoint_limit_error(
                self.limits.max_endpoints_per_snapshot,
                order + 1,
            ));
        }
        self.endpoints += 1;
        Ok(())
    }

    /// Charges one session, failing explicitly past the snapshot-wide session cap.
    ///
    /// This is charged as soon as a session is read, before its record is retained, so a
    /// session whose own reads fail still counts against the snapshot allowance.
    pub(crate) fn charge_session(&mut self, order: usize) -> Result<(), RecorderError> {
        if self.sessions >= self.limits.max_sessions_per_snapshot {
            return Err(session_limit_error(
                self.limits.max_sessions_per_snapshot,
                order + 1,
            ));
        }
        self.sessions += 1;
        Ok(())
    }

    /// Charges `count` owned bytes, failing explicitly past the payload cap.
    ///
    /// The check runs before the storage is allocated, so a snapshot that would exceed
    /// the bound fails instead of allocating and then reporting.
    pub(crate) fn charge_bytes(&mut self, count: usize, what: &str) -> Result<(), RecorderError> {
        let total = self.charged_bytes.checked_add(count).ok_or_else(|| {
            payload_overflow_error(&format!(
                "the running total for Frame-owned snapshot storage overflowed while charging {what}"
            ))
        })?;
        if total > self.limits.max_snapshot_payload_bytes {
            return Err(payload_limit_error(
                self.limits.max_snapshot_payload_bytes,
                total,
                what,
            ));
        }
        self.charged_bytes = total;
        Ok(())
    }

    /// Reserves the capacity for one owned buffer before it is allocated, and returns the
    /// charged byte count.
    ///
    /// `elements * element_capacity_bytes` is rounded up to the next power of two, because
    /// a `Vec`/`String` growth strategy and the allocator can both round the real
    /// allocation above the requested capacity. Callers check reported capacity and
    /// charge any excess; this reservation supplies headroom before allocation.
    pub(crate) fn reserve_capacity(
        &mut self,
        elements: usize,
        element_capacity_bytes: usize,
        what: &str,
    ) -> Result<usize, RecorderError> {
        let exact = elements.checked_mul(element_capacity_bytes).ok_or_else(|| {
            payload_overflow_error(&format!(
                "the requested capacity for {what} overflowed {elements} elements at {element_capacity_bytes} bytes"
            ))
        })?;
        let rounded = exact.checked_next_power_of_two().unwrap_or(usize::MAX);
        self.charge_bytes(rounded, what)?;
        Ok(rounded)
    }
}

/// The endpoint cap was exceeded; no endpoint beyond it is read or allocated.
pub(crate) fn endpoint_limit_error(max_endpoints: usize, observed: usize) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message: format!(
            "Windows active render endpoints exceed the {max_endpoints}-endpoint session-discovery bound (at least {observed} were reported), so this snapshot is refused rather than silently truncated."
        ),
        recovery: RecoveryStatus::NoRecording,
        action: format!(
            "Retry application-session discovery; if this machine legitimately has more than {max_endpoints} active render endpoints, the session-discovery bound has to be raised deliberately."
        ),
    }
}

/// The snapshot-wide session cap was exceeded.
pub(crate) fn session_limit_error(max_sessions: usize, observed: usize) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message: format!(
            "Windows session enumeration exceeds the {max_sessions}-session session-discovery bound (at least {observed} sessions were reported), so this snapshot is refused rather than silently truncated."
        ),
        recovery: RecoveryStatus::NoRecording,
        action: format!(
            "Retry application-session discovery; if this machine legitimately has more than {max_sessions} audio sessions, the session-discovery bound has to be raised deliberately."
        ),
    }
}

/// One retained native string exceeds the per-string bound.
pub(crate) fn string_limit_error(max_units: usize, what: &str, units: usize) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message: format!(
            "Windows reported {units} UTF-16 content units for {what}, past the {max_units}-unit per-string session-discovery bound, so this snapshot is refused rather than silently truncated."
        ),
        recovery: RecoveryStatus::NoRecording,
        action: "Retry application-session discovery; if this persists, report the owning application and the Windows build.".to_string(),
    }
}

/// A retained native string that is not valid UTF-16.
///
/// Identity and path data must convert exactly; a lossy replacement would let two
/// different sessions or processes collide on one identity.
pub(crate) fn malformed_utf16_error(what: &str, units: usize) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message: format!(
            "Windows reported {units} UTF-16 units for {what} that are not valid Unicode, so no identity or identifier is invented from them."
        ),
        recovery: RecoveryStatus::NoRecording,
        action: "Retry application-session discovery; if it persists, report the owning application and the Windows build.".to_string(),
    }
}

/// Frame-owned snapshot storage exceeds the payload bound.
pub(crate) fn payload_limit_error(max_bytes: usize, total: usize, what: &str) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message: format!(
            "Frame-owned snapshot storage would reach {total} bytes while charging {what}, past the {max_bytes}-byte session-discovery bound, so this snapshot is refused rather than silently truncated."
        ),
        recovery: RecoveryStatus::NoRecording,
        action: "Retry application-session discovery with fewer concurrent audio sessions."
            .to_string(),
    }
}

/// Frame-owned snapshot accounting overflowed its own checked arithmetic.
pub(crate) fn payload_overflow_error(detail: &str) -> RecorderError {
    RecorderError {
        code: ErrorCode::Internal,
        message: format!(
            "Session-discovery payload accounting failed: {detail}. No snapshot is reported from inconsistent accounting."
        ),
        recovery: RecoveryStatus::NoRecording,
        action: "Retry application-session discovery; if it persists, report the error."
            .to_string(),
    }
}
