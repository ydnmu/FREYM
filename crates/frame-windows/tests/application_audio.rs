// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
#![cfg(target_os = "windows")]
use frame_audio::{AudioCancellation, AudioCaptureBackend, AudioContext};
use frame_clock::MasterClock;
use frame_contracts::{AudioSourceId, ErrorCode};
use frame_windows::{PlatformBackend, WindowsAudioSession};
use std::sync::Arc;
#[test]
fn cancelled_application_start_never_activates_native_capture() {
    let cancellation = AudioCancellation::default();
    cancellation.cancel();
    let result: Result<WindowsAudioSession, _> = PlatformBackend.start_audio(
        &AudioSourceId("wasapi-session:1:61:1:62:".into()),
        AudioContext {
            clock: Arc::new(MasterClock::default()),
            cancellation,
        },
    );
    assert_eq!(result.err().unwrap().code, ErrorCode::InvalidTransition);
}
#[test]
fn malformed_application_source_is_refused_without_fallback() {
    let result = PlatformBackend.start_audio(
        &AudioSourceId("wasapi-session:not-an-identity".into()),
        AudioContext {
            clock: Arc::new(MasterClock::default()),
            cancellation: AudioCancellation::default(),
        },
    );
    assert_eq!(result.err().unwrap().code, ErrorCode::InvalidRequest);
}
