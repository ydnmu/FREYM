//! Application services and media owners. No dependency on a UI framework.
pub mod audio_discovery;
#[cfg(target_os = "windows")]
pub mod audio_levels;
#[cfg(target_os = "windows")]
pub mod av_recording;
pub mod discovery;
#[cfg(target_os = "windows")]
pub mod encoder_calibration;
#[cfg(target_os = "windows")]
mod encoder_restart;
#[cfg(target_os = "macos")]
pub mod macos_recording;
#[cfg(target_os = "windows")]
pub mod metadata;
#[cfg(target_os = "windows")]
pub mod preview_transport;
pub mod recording_control;
pub mod recording_files;
pub mod recording_health;
pub mod recovery;
mod replay_worker;
pub mod settings;
#[cfg(target_os = "windows")]
pub mod video_recording;
