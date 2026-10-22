//! macOS 14+ ScreenCaptureKit, VideoToolbox and AVFoundation backend.
#[cfg(any(target_os = "macos", feature = "native-typecheck", test))]
mod media_contract;
#[cfg(any(target_os = "macos", feature = "native-typecheck"))]
mod native;
#[cfg(any(target_os = "macos", feature = "native-typecheck"))]
pub use native::*;

#[cfg(not(any(target_os = "macos", feature = "native-typecheck")))]
mod unavailable {
    use frame_contracts::{AudioSource, DisplayInfo, RecorderError, WindowInfo};
    pub struct PlatformBackend;
    impl frame_capture::DisplayBackend for PlatformBackend {
        fn enumerate_displays(&self) -> Result<Vec<DisplayInfo>, RecorderError> {
            Err(RecorderError::unsupported("macos display enumeration"))
        }
        fn enumerate_windows(&self) -> Result<Vec<WindowInfo>, RecorderError> {
            Err(RecorderError::unsupported("macos window enumeration"))
        }
    }
    impl frame_audio::AudioBackend for PlatformBackend {
        fn enumerate_audio_sources(&self) -> Result<Vec<AudioSource>, RecorderError> {
            Err(RecorderError::unsupported("macos audio enumeration"))
        }
    }
    impl frame_encode::EncoderProbe for PlatformBackend {
        fn probe(&self) -> Result<Vec<frame_encode::EncoderCapability>, RecorderError> {
            Err(RecorderError::unsupported("macos encoder probing"))
        }
    }
}
#[cfg(not(any(target_os = "macos", feature = "native-typecheck")))]
pub use unavailable::*;
