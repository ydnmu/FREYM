//! Linux portal capture, PipeWire microphone input and VA-API H.264 recording.
//! ScreenCast offers a picker, not an inventory of monitors. `PORTAL_PICKER_ID`
//! identifies that action and can only be resolved by a fresh portal grant.
mod video_contract;
pub use video_contract::{DmaBufBlocker, DmaDrmFormat, DrmDeviceIdentity, assess_dma_buf_import};
#[cfg(all(target_os = "linux", frame_linux_pulse))]
mod application_audio;
#[cfg(target_os = "linux")]
mod audio;
#[cfg(target_os = "linux")]
mod dma_buf;
#[cfg(target_os = "linux")]
mod portal;
#[cfg(target_os = "linux")]
mod recording;
#[cfg(target_os = "linux")]
mod video;

#[cfg(target_os = "linux")]
pub use recording::{LinuxRecording, LinuxRecordingConfig, RecordingProgress};
#[cfg(target_os = "linux")]
pub use video::{EncoderPath, probe_encoder, probe_encoder_for_codec};

use frame_audio::AudioBackend;
use frame_capture::DisplayBackend;
#[cfg(target_os = "linux")]
use frame_contracts::DisplayId;
use frame_contracts::{AudioSource, DisplayInfo, RecorderError, WindowInfo};
use frame_encode::{EncoderCapability, EncoderProbe};

pub const PORTAL_PICKER_ID: &str = "linux:portal:choose-screen";

pub struct PlatformBackend;

pub fn application_audio_supported() -> bool {
    #[cfg(all(target_os = "linux", frame_linux_pulse))]
    {
        application_audio::supported()
    }
    #[cfg(not(all(target_os = "linux", frame_linux_pulse)))]
    {
        false
    }
}

#[cfg(target_os = "linux")]
impl DisplayBackend for PlatformBackend {
    fn enumerate_displays(&self) -> Result<Vec<DisplayInfo>, RecorderError> {
        if !portal::session_available() {
            return Ok(Vec::new());
        }
        // This is a picker action, not a claim about discovered monitors or the
        // stream the user will grant. Geometry arrives only after Start succeeds.
        Ok(vec![DisplayInfo {
            id: DisplayId(PORTAL_PICKER_ID.into()),
            name: "Choose a screen with the system picker".into(),
            width: 0,
            height: 0,
            x: 0,
            y: 0,
            scale_factor: 1.0,
            refresh_rate_hz: None,
            is_primary: false,
            hdr_available: false,
            required_recording_quality: None,
        }])
    }

    fn enumerate_windows(&self) -> Result<Vec<WindowInfo>, RecorderError> {
        // Wayland does not expose a global window list. The portal picker may
        // offer windows, but they are not persistent WindowInfo identities.
        Ok(Vec::new())
    }
}

#[cfg(target_os = "linux")]
impl AudioBackend for PlatformBackend {
    fn enumerate_audio_sources(&self) -> Result<Vec<AudioSource>, RecorderError> {
        let microphones = audio::enumerate_microphones();
        let sources = microphones.as_ref().cloned().unwrap_or_default();
        #[cfg(frame_linux_pulse)]
        let sources = {
            let mut sources = sources;
            sources.extend(application_audio::enumerate());
            sources
        };
        if sources.is_empty() {
            microphones?;
        }
        Ok(sources)
    }
}

#[cfg(target_os = "linux")]
impl EncoderProbe for PlatformBackend {
    fn probe(&self) -> Result<Vec<EncoderCapability>, RecorderError> {
        let mut candidates: Vec<_> = probe_encoder()?
            .into_iter()
            .map(|path| path.capability())
            .collect();
        if let Some(path) = probe_encoder_for_codec(true)? {
            candidates.push(path.capability());
        }
        Ok(candidates)
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn error(
    code: frame_contracts::ErrorCode,
    message: impl Into<String>,
    action: impl Into<String>,
) -> RecorderError {
    RecorderError {
        code,
        message: message.into(),
        recovery: frame_contracts::RecoveryStatus::NoRecording,
        action: action.into(),
    }
}

#[cfg(not(target_os = "linux"))]
impl DisplayBackend for PlatformBackend {
    fn enumerate_displays(&self) -> Result<Vec<DisplayInfo>, RecorderError> {
        Err(RecorderError::unsupported("Linux display discovery"))
    }
    fn enumerate_windows(&self) -> Result<Vec<WindowInfo>, RecorderError> {
        Err(RecorderError::unsupported("Linux window discovery"))
    }
}
#[cfg(not(target_os = "linux"))]
impl AudioBackend for PlatformBackend {
    fn enumerate_audio_sources(&self) -> Result<Vec<AudioSource>, RecorderError> {
        Err(RecorderError::unsupported("Linux microphone discovery"))
    }
}
#[cfg(not(target_os = "linux"))]
impl EncoderProbe for PlatformBackend {
    fn probe(&self) -> Result<Vec<EncoderCapability>, RecorderError> {
        Err(RecorderError::unsupported("Linux encoder probing"))
    }
}
