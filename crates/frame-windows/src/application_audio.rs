// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Selected process-tree loopback. Discovery IDs select a live process instance,
//! not a persistent executable route or an isolated individual WASAPI session.
mod native;
use crate::application_sessions::ApplicationProcessAvailability;
use frame_audio::{AudioContext, AudioSession, DrainableAudioSession, PcmBlock, PcmDrainPoll};
use frame_contracts::{AudioSourceId, ErrorCode, RecorderError, RecoveryStatus};

pub struct WindowsApplicationAudioSession(crate::WindowsMicrophoneSession);
/// Actual OS build used by process-loopback admission; no manifest version guess.
pub fn windows_build_number() -> Result<u32, RecorderError> {
    native::build_number()
}
impl WindowsApplicationAudioSession {
    pub fn diagnostics(&self) -> Result<String, RecorderError> {
        self.0.diagnostics().map(|text| {
            format!(
                "process_tree_loopback=true\ndevice_position_available=false\nqpc_interval_tolerance_ns=1000000\n{}",
                text.replace("microphone", "audio")
            )
        })
    }
}
impl AudioSession for WindowsApplicationAudioSession {
    fn pcm_channels(&self) -> std::num::NonZeroU16 {
        self.0.pcm_channels()
    }

    fn try_pcm(&mut self) -> Result<Option<PcmBlock>, RecorderError> {
        self.0.try_pcm()
    }
    fn stop(&mut self) -> Result<(), RecorderError> {
        self.0.stop()
    }
    fn stop_for_continuity(&mut self) -> Result<(), RecorderError> {
        self.0.stop_for_continuity()
    }
}
impl DrainableAudioSession for WindowsApplicationAudioSession {
    fn request_drain(&mut self) -> Result<(), RecorderError> {
        self.0.request_drain()
    }
    fn poll_drain(&mut self) -> Result<PcmDrainPoll, RecorderError> {
        self.0.poll_drain()
    }
}
pub(crate) fn error(code: ErrorCode, message: impl Into<String>) -> RecorderError {
    RecorderError { code, message: message.into(), recovery: RecoveryStatus::NoRecording,
        action: "Re-select a live single-process audio session and retry; no system-mix fallback is used.".into() }
}
pub(crate) fn lost(message: impl Into<String>) -> RecorderError {
    error(ErrorCode::SourceLost, message)
}

pub(crate) fn validate_identity(
    availability: ApplicationProcessAvailability,
    pid: Option<u32>,
    creation: Option<u64>,
    system: bool,
    state: i32,
    mapped: bool,
) -> Result<(u32, u64), RecorderError> {
    if availability == ApplicationProcessAvailability::MultiProcessSession {
        return Err(error(
            ErrorCode::NotImplemented,
            "Selected session has multiprocess attribution.",
        ));
    }
    if availability != ApplicationProcessAvailability::SingleProcess
        || system
        || state == 2
        || !mapped
    {
        return Err(lost(
            "Selected application session is no longer captureable.",
        ));
    }
    let pid = pid
        .filter(|value| *value != 0)
        .ok_or_else(|| lost("Selected session has no single nonzero process."))?;
    let creation = creation.ok_or_else(|| {
        error(
            ErrorCode::PermissionDenied,
            "Required process creation time is unavailable.",
        )
    })?;
    Ok((pid, creation))
}
pub(crate) fn compare_identity(
    expected: (u32, u64),
    actual: (u32, u64),
) -> Result<(), RecorderError> {
    if expected != actual {
        Err(lost(
            "Selected process identity changed or its PID was reused.",
        ))
    } else {
        Ok(())
    }
}
/// First-sample QPC must advance with packet duration within 1 ms. This bounds
/// gross timing gaps; it cannot establish sample-exact missing-frame detection.
pub(crate) fn validate_qpc_interval(
    previous: u64,
    frames: u32,
    current: u64,
    rate: u32,
) -> Result<(), RecorderError> {
    let duration = u64::from(frames) * 1_000_000_000 / u64::from(rate);
    let actual = current
        .checked_sub(previous)
        .ok_or_else(|| lost("Process QPC moved backwards."))?;
    if actual == 0 || actual.abs_diff(duration) > 1_000_000 {
        return Err(lost(format!(
            "Process QPC interval {actual} ns differs from preceding packet duration {duration} ns beyond the 1000000 ns proof tolerance."
        )));
    }
    Ok(())
}
pub(crate) fn start(
    source: &AudioSourceId,
    context: AudioContext,
) -> Result<WindowsApplicationAudioSession, RecorderError> {
    if source.0.len() > 65_600 {
        return Err(RecorderError::invalid(
            "Application-session identity exceeds the discovery bounds.",
        ));
    }
    let Some((endpoint, instance)) =
        crate::application_sessions::decode_application_session_id(&source.0)
    else {
        return Err(RecorderError::invalid(
            "Malformed application-session identity.",
        ));
    };
    if endpoint.encode_utf16().count() > 4096 || instance.encode_utf16().count() > 4096 {
        return Err(RecorderError::invalid(
            "Application-session component exceeds 4096 UTF-16 units.",
        ));
    }
    let selected = source.clone();
    crate::microphone::start_prepared(source, context, move |checkpoint| {
        native::prepare(&selected, false, checkpoint)
    })
    .map(WindowsApplicationAudioSession)
}
/// Process-loopback complement of exactly one explicitly selected process tree.
pub(crate) fn start_complement(
    source: &AudioSourceId,
    context: AudioContext,
) -> Result<WindowsApplicationAudioSession, RecorderError> {
    let selected = crate::system_audio::excluded_source(source)
        .ok_or_else(|| RecorderError::invalid("Invalid other-system identity"))?;
    crate::microphone::start_prepared(source, context, move |checkpoint| {
        native::prepare(&selected, true, checkpoint)
    })
    .map(WindowsApplicationAudioSession)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn process_qpc_policy_keeps_real_timestamps_and_refuses_gross_gaps() {
        assert!(validate_qpc_interval(20_000_000, 480, 30_000_100, 48_000).is_ok());
        assert!(validate_qpc_interval(20_000_000, 480, 40_000_000, 48_000).is_err());
        assert!(validate_qpc_interval(20_000_000, 480, 20_000_000, 48_000).is_err());
    }
    #[test]
    fn ambiguous_missing_expired_and_reused_process_identities_are_refused() {
        use ApplicationProcessAvailability::*;
        assert_eq!(
            validate_identity(MultiProcessSession, Some(7), Some(1), false, 1, true)
                .unwrap_err()
                .code,
            ErrorCode::NotImplemented
        );
        assert!(validate_identity(SingleProcess, Some(7), Some(1), false, 2, true).is_err());
        assert_eq!(
            validate_identity(SingleProcess, Some(7), None, false, 1, true)
                .unwrap_err()
                .code,
            ErrorCode::PermissionDenied
        );
        assert!(compare_identity((7, 1), (7, 2)).is_err());
        assert!(compare_identity((7, 1), (7, 1)).is_ok());
    }
}
