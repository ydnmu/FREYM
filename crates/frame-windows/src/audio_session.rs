// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Native composition; shared audio contracts remain unchanged.
use crate::{WindowsApplicationAudioSession, WindowsMicrophoneSession};
use frame_audio::{
    AudioCaptureBackend, AudioContext, AudioSession, DrainableAudioSession, PcmBlock, PcmDrainPoll,
};
use frame_contracts::{AudioSourceId, RecorderError};

pub enum WindowsAudioSession {
    Microphone(WindowsMicrophoneSession),
    Application(WindowsApplicationAudioSession),
    System(WindowsMicrophoneSession),
}
impl WindowsAudioSession {
    pub fn diagnostics(&self) -> Result<String, RecorderError> {
        match self {
            Self::Microphone(value) | Self::System(value) => value.diagnostics(),
            Self::Application(value) => value.diagnostics(),
        }
    }
}
impl AudioSession for WindowsAudioSession {
    fn allows_silent_gaps(&self) -> bool {
        matches!(self, Self::System(_))
    }

    fn pcm_channels(&self) -> std::num::NonZeroU16 {
        match self {
            Self::Microphone(s) | Self::System(s) => s.pcm_channels(),
            Self::Application(s) => s.pcm_channels(),
        }
    }

    fn try_pcm(&mut self) -> Result<Option<PcmBlock>, RecorderError> {
        match self {
            Self::Microphone(value) | Self::System(value) => value.try_pcm(),
            Self::Application(value) => value.try_pcm(),
        }
    }
    fn stop(&mut self) -> Result<(), RecorderError> {
        match self {
            Self::Microphone(value) | Self::System(value) => value.stop(),
            Self::Application(value) => value.stop(),
        }
    }
    fn stop_for_continuity(&mut self) -> Result<(), RecorderError> {
        match self {
            Self::Microphone(value) | Self::System(value) => value.stop_for_continuity(),
            Self::Application(value) => value.stop_for_continuity(),
        }
    }
}
impl DrainableAudioSession for WindowsAudioSession {
    fn request_drain(&mut self) -> Result<(), RecorderError> {
        match self {
            Self::Microphone(value) | Self::System(value) => value.request_drain(),
            Self::Application(value) => value.request_drain(),
        }
    }
    fn poll_drain(&mut self) -> Result<PcmDrainPoll, RecorderError> {
        match self {
            Self::Microphone(value) | Self::System(value) => value.poll_drain(),
            Self::Application(value) => value.poll_drain(),
        }
    }
}
impl AudioCaptureBackend for crate::PlatformBackend {
    type Session = WindowsAudioSession;
    fn validate_capture_routes(
        &mut self,
        routes: &[frame_contracts::AudioRouteSelection],
    ) -> Result<(), RecorderError> {
        use frame_audio::AudioBackend;
        frame_settings::audio_admission::validate_audio_selection(
            routes,
            &self.enumerate_audio_sources()?,
            crate::windows_build_number()? >= 20348,
            true,
        )
    }
    fn reopen_audio_factory(&self) -> Option<frame_audio::AudioReopenFactory<Self::Session>> {
        Some(|source, context| crate::PlatformBackend.start_audio(source, context))
    }
    fn start_audio(
        &mut self,
        source: &AudioSourceId,
        context: AudioContext,
    ) -> Result<Self::Session, RecorderError> {
        if source.0.starts_with("wasapi-mic:") {
            crate::microphone::start_microphone(source, context)
                .map(WindowsAudioSession::Microphone)
        } else if let Some(endpoint) = crate::system_audio::endpoint(source) {
            let endpoint = endpoint.to_owned();
            crate::microphone::start_prepared(source, context, move |checkpoint| {
                crate::microphone::native::prepare_loopback(&endpoint, checkpoint)
            })
            .map(WindowsAudioSession::System)
        } else if source.0.starts_with(crate::system_audio::REST_PREFIX) {
            crate::application_audio::start_complement(source, context)
                .map(WindowsAudioSession::Application)
        } else if source.0.starts_with("wasapi-session:") {
            crate::application_audio::start(source, context).map(WindowsAudioSession::Application)
        } else {
            Err(RecorderError::invalid(
                "Unsupported Windows audio source identity.",
            ))
        }
    }
}
