//! Blocking native discovery. Invoke on an application background executor.
use frame_contracts::{EngineSnapshot, RecorderError};

pub fn snapshot() -> Result<EngineSnapshot, RecorderError> {
    let mut snapshot = frame_core::foundation_snapshot();
    #[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
    {
        use frame_audio::AudioBackend as _;
        use frame_capture::DisplayBackend as _;
        #[cfg(target_os = "windows")]
        let backend = frame_windows::PlatformBackend;
        #[cfg(target_os = "macos")]
        let backend = frame_macos::PlatformBackend::default();
        #[cfg(target_os = "linux")]
        let backend = frame_linux::PlatformBackend;
        snapshot.displays = backend.enumerate_displays()?;
        snapshot.audio_sources = backend.enumerate_audio_sources()?;
        #[cfg(target_os = "windows")]
        {
            snapshot.windows = backend.enumerate_windows()?;
            for display in &mut snapshot.displays {
                // The output's active HDR signal is capture-source information.
                // No encoder qualification or replay-export result is folded in.
                display.hdr_available =
                    frame_windows::active_hdr_display(&display.id)? == Some(true);
                display.required_recording_quality =
                    frame_windows::display_capture_adapter_identity(&display.id)
                        .ok()
                        .and_then(|adapter| {
                            frame_settings::policy::windows_adapter_required_quality(
                                adapter.vendor_id,
                            )
                        });
            }
        }
        #[cfg(target_os = "macos")]
        if frame_macos::screen_authorized() {
            snapshot.windows = backend.enumerate_windows()?;
        } else {
            snapshot.warnings.push(frame_contracts::EngineWarning {
                code: "macosScreenPermissionRequired".into(),
                message: "Screen Recording access has not been granted.".into(),
                action: "Press Record on a display to request access, then refresh sources.".into(),
            });
        }
    }
    snapshot.capabilities = crate::recording_control::capabilities(&snapshot.displays);
    #[cfg(target_os = "windows")]
    crate::encoder_calibration::schedule_displays(&snapshot.displays);
    Ok(snapshot)
}
