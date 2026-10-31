// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Real Windows application audio session discovery tests (FRAME-0106A).
//!
//! These tests call the production entry points against the local machine and assert
//! platform-independent invariants, never a particular application being open, a
//! particular session count or a particular audio device. A session list that happens to
//! be empty is a valid result here, so these tests never claim that a populated list was
//! observed: real-machine behavior requires separate device validation.
//!
//! Every test here is a snapshot diagnostic, not hardware acceptance. None of them
//! captures audio, and none of them proves per-application isolation.
#![cfg(target_os = "windows")]

use std::collections::HashSet;

use frame_contracts::{AudioRoute, AudioSourceKind};

/// Reads one snapshot through the public discovery entry point.
fn sources() -> Vec<frame_contracts::AudioSource> {
    frame_windows::enumerate_application_audio_sources()
        .expect("application audio session discovery must not fail on a Windows desktop")
}

#[test]
fn discovery_reports_only_canonical_application_sources() {
    for source in sources() {
        assert_eq!(source.kind, AudioSourceKind::Application);
        assert_eq!(
            source.route,
            AudioRoute::Ignore,
            "session discovery plans no route; the ADR requires Ignore"
        );
        assert!(
            source.id.0.starts_with("wasapi-session:"),
            "unexpected identity {}",
            source.id.0
        );
        assert!(
            !source.display_name.is_empty(),
            "a source must carry a readable label, never an empty one"
        );
        if let Some(identifier) = &source.app_identifier {
            assert!(
                identifier.starts_with("win-exe:"),
                "app_identifier must be the documented prefix, got {identifier}"
            );
            assert!(
                identifier.len() > "win-exe:".len(),
                "app_identifier must carry a path, got {identifier}"
            );
        }
        if let Some(pid) = source.process_id {
            assert!(pid != 0, "a reported process id must be a usable process");
        }
    }
}

#[test]
fn snapshot_identities_are_unique() {
    let found = sources();
    let unique: HashSet<_> = found.iter().map(|source| &source.id.0).collect();
    assert_eq!(unique.len(), found.len());
}

#[test]
fn snapshots_are_ordered_by_full_identity() {
    let found = sources();
    assert!(found.windows(2).all(|pair| pair[0].id < pair[1].id));
}

#[test]
fn activity_matches_the_same_snapshot_native_state() {
    let snapshot = frame_windows::application_audio_session_snapshot().unwrap();
    for source in &snapshot.sources {
        let record = snapshot
            .endpoints
            .iter()
            .flat_map(|endpoint| &endpoint.sessions)
            .find(|record| record.source_id.as_ref() == Some(&source.id))
            .expect("each canonical source comes from one recorded session");
        assert!(record.skip_reason.is_none());
        assert!(!record.is_system_sounds);
        assert!(matches!(record.raw_state, 0 | 1));
        assert_eq!(source.is_active, record.raw_state == 1);
        assert_eq!(source.is_active, record.is_active);
    }
}

#[test]
fn diagnostics_report_is_produced_and_self_describing() {
    let report = frame_windows::application_audio_diagnostics()
        .expect("application audio session diagnostics must not fail on a Windows desktop");
    for expected in [
        "Frame application audio session diagnostics (FRAME-0106A)",
        "read-only diagnostic snapshot",
        "captures no audio",
        "FRAME-0106B",
        "not atomic",
        "not a live tracker",
        "A source ID is a live-session selection identity",
        "is_active is the OS session state",
        "Bounds (exceeding any of these fails the snapshot, it never truncates)",
        "Endpoints and session records (raw evidence)",
    ] {
        assert!(
            report.contains(expected),
            "the diagnostics report must contain {expected:?}"
        );
    }
    // The report never claims a capability this task does not implement.
    for forbidden in [
        "per-application capture is implemented",
        "records audio",
        "starts capturing",
    ] {
        assert!(
            !report.contains(forbidden),
            "the report must not claim {forbidden:?}"
        );
    }
}

#[test]
fn combined_backend_returns_microphones_and_application_sources() {
    use frame_audio::AudioBackend;
    let found = frame_windows::PlatformBackend
        .enumerate_audio_sources()
        .unwrap();
    for source in &found {
        let prefix = match source.kind {
            AudioSourceKind::Microphone => "wasapi-mic:",
            AudioSourceKind::Application => "wasapi-session:",
            _ => panic!("unexpected source kind: {:?}", source.kind),
        };
        assert!(source.id.0.starts_with(prefix));
        assert_eq!(source.route, AudioRoute::Ignore);
    }
    assert!(found.windows(2).all(|pair| pair[0].id < pair[1].id));
}
