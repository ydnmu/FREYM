//! Audio settings discovery without display, preview or encoder calibration work.
use frame_contracts::{AudioSource, RecorderError};

pub struct AudioDiscovery {
    pub sources: Vec<AudioSource>,
    pub audio_levels: bool,
    pub per_app_audio: bool,
}

/// Blocking native enumeration only; run on a control/background executor.
/// No audio session is opened and no default device is substituted.
pub fn discover() -> Result<AudioDiscovery, RecorderError> {
    #[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
    let sources = {
        use frame_audio::AudioBackend;
        #[cfg(target_os = "windows")]
        let backend = frame_windows::PlatformBackend;
        #[cfg(target_os = "macos")]
        let backend = frame_macos::PlatformBackend::default();
        #[cfg(target_os = "linux")]
        let backend = frame_linux::PlatformBackend;
        backend.enumerate_audio_sources()?
    };
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    let sources = Vec::new();
    // With no displays the existing capability probes short-circuit video work.
    let capabilities = crate::recording_control::capabilities(&[]);
    Ok(AudioDiscovery {
        sources,
        audio_levels: capabilities.audio_levels,
        per_app_audio: capabilities.per_app_audio,
    })
}
