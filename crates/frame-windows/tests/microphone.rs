// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Public microphone session contract tests (FRAME-0105).
//!
//! These exercise only the accepted public surface -- enumeration identity, the
//! human-readable diagnostics and non-recording `start_audio` rejections. They never open a
//! selected endpoint for capture, so no microphone is recorded in default `cargo test`.

#![cfg(target_os = "windows")]

use frame_audio::{AudioBackend, AudioCancellation, AudioCaptureBackend, AudioContext};
use frame_clock::MasterClock;
use frame_contracts::{AudioSourceId, AudioSourceKind, ErrorCode};
use frame_windows::{PlatformBackend, microphone_diagnostics};
use std::sync::Arc;

fn context() -> AudioContext {
    AudioContext {
        clock: Arc::new(MasterClock::default()),
        cancellation: AudioCancellation::default(),
    }
}

#[test]
fn enumeration_reports_canonical_microphone_identities() {
    let sources: Vec<_> = PlatformBackend
        .enumerate_audio_sources()
        .unwrap()
        .into_iter()
        .filter(|source| source.kind == AudioSourceKind::Microphone)
        .collect();
    for source in &sources {
        assert_eq!(source.kind, AudioSourceKind::Microphone);
        assert!(
            source.id.0.starts_with("wasapi-mic:"),
            "identity must be the canonical opaque endpoint id: {}",
            source.id.0
        );
    }
    // Deterministic ordering by full ID, and no duplicate identities.
    for pair in sources.windows(2) {
        assert!(pair[0].id < pair[1].id, "ids must be sorted and unique");
    }
}

#[test]
fn microphone_diagnostics_is_readable_and_non_empty() {
    let text = microphone_diagnostics().unwrap();
    assert!(text.contains("microphone_endpoints"), "{text}");
}

#[test]
fn a_non_microphone_identity_is_rejected_without_touching_a_device() {
    let result = PlatformBackend.start_audio(&AudioSourceId("display:1".to_string()), context());
    let error = result.err().expect("a non-microphone identity is refused");
    assert_eq!(error.code, ErrorCode::InvalidRequest);
}

#[test]
fn a_cancelled_start_refuses_before_any_native_preparation() {
    let cancelled = AudioContext {
        clock: Arc::new(MasterClock::default()),
        cancellation: AudioCancellation::default(),
    };
    cancelled.cancellation.cancel();
    let result = PlatformBackend.start_audio(
        &AudioSourceId("wasapi-mic:{never-opened}".to_string()),
        cancelled,
    );
    let error = result.err().expect("a cancelled start is refused");
    assert_eq!(error.code, ErrorCode::InvalidTransition);
}
