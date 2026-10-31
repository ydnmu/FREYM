//! Windows display capture, hardware H.264 encoding, selected microphone/process
//! audio capture and source discovery. Native handles stay inside this crate.
//! Desktop orchestration and container/audio processing live in their host/shared
//! crates. Experimental window capture remains outside production admission; region capture copies a
//! rectangle of an already captured display surface on the GPU (FRAME-0205).
use frame_contracts::{AudioSource, DisplayInfo, RecorderError, WindowInfo};
#[allow(unsafe_code)]
#[cfg(target_os = "windows")]
pub mod metadata_observer;
#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
pub mod performance;
#[allow(unsafe_code)]
#[cfg(target_os = "windows")]
pub mod recording_storage;

#[cfg(target_os = "windows")]
mod capture;

#[cfg(target_os = "windows")]
mod display;

#[cfg(target_os = "windows")]
mod encode;

#[cfg(target_os = "windows")]
mod witness;

// FRAME-0105: the Windows microphone backend. The module owns the WASAPI session, its
// bounded queue, the device-format conversion rules and its diagnostics; it implements
// `AudioCaptureBackend` for `PlatformBackend` so the accepted audio contracts are
// unchanged and no native handle or COM type crosses the API.
#[cfg(target_os = "windows")]
mod microphone;

#[cfg(target_os = "windows")]
mod application_audio;
#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
mod audio_identity;
#[cfg(target_os = "windows")]
mod audio_session;
#[cfg(target_os = "windows")]
mod system_audio;
#[cfg(target_os = "windows")]
pub use application_audio::WindowsApplicationAudioSession;
#[cfg(target_os = "windows")]
pub use application_audio::windows_build_number;
#[cfg(target_os = "windows")]
pub use audio_session::WindowsAudioSession;

// Read-only session metadata; process capture remains separate.
#[cfg(target_os = "windows")]
mod application_sessions;

// The rate rules are pure and platform independent, but only the Windows backend
// consumes them: for production they are compiled on Windows, and elsewhere they are
// compiled for test builds only. That keeps the rules covered on every host without
// leaving unused code in a non-Windows production build and without suppressing a
// warning.
#[cfg(any(target_os = "windows", test))]
mod display_rate;

#[cfg(target_os = "windows")]
mod surface;
pub use capture::active_hdr_display;
pub use surface::{SurfaceFormat, SurfaceSignal};

#[allow(unsafe_code)]
#[cfg(target_os = "windows")]
pub mod camera;

#[allow(unsafe_code)]
#[cfg(target_os = "windows")]
pub mod compositor;

#[cfg(target_os = "windows")]
mod color_convert;

#[cfg(target_os = "windows")]
mod window_capture;
#[cfg(target_os = "windows")]
mod window_discovery;
#[cfg(target_os = "windows")]
mod window_picker;
#[cfg(target_os = "windows")]
pub use window_picker::pick_window_on_screen;

#[cfg(target_os = "windows")]
pub use window_capture::WINDOW_FRAME_POOL_BUFFERS;
#[cfg(target_os = "windows")]
pub use window_capture::WindowsWindowCaptureSession;
pub use window_capture::{
    WindowRecordingSource, recording_source as window_recording_source, resolve_game_window,
};
#[cfg(target_os = "windows")]
pub use window_discovery::window_diagnostics;
#[cfg(target_os = "windows")]
pub fn validate_window_selection(
    id: &frame_contracts::WindowId,
) -> Result<frame_contracts::WindowInfo, frame_contracts::RecorderError> {
    window_discovery::validate_selection(id)
}

#[allow(unsafe_code)]
#[cfg(target_os = "windows")]
pub mod cursor_preview_diagnostics;
#[cfg(target_os = "windows")]
pub mod gpu_preview;
/// Bounded thumbnails from the running display capture worker.
#[cfg(target_os = "windows")]
pub mod preview;

#[cfg(target_os = "windows")]
pub use capture::{
    ThumbnailPreviewSession, start_display_with_cursor, start_preview_session,
    start_preview_session_with_cursor, start_thumbnail_preview_session,
};

// FRAME-0205: GPU region (crop) capture. The module owns the crop texture pool, the
// rectangle-to-D3D11-box mapping and the derived lease that keeps its source alive. It is
// the second reviewed FFI boundary in the crate, and it reuses the accepted display
// session and [`WindowsSurface`] lease instead of adding another capture path; the display
// capture algorithm itself is unchanged.
#[allow(unsafe_code)]
#[cfg(target_os = "windows")]
mod region;

/// Human-readable display diagnostics for the read-only probe (FRAME-0102).
///
/// Diagnostic text for a person to read, not a data API: it returns the report as a
/// string and exposes no native rate or association type. It is produced from the
/// same single observation as `DisplayBackend::enumerate_displays`.
#[cfg(target_os = "windows")]
pub use display::display_diagnostics;

/// One running Windows display capture session (FRAME-0103).
///
/// The only added public capture types are this session and its opaque, non-cloneable
/// [`WindowsSurface`] lease; no COM object, worker queue or native timestamp model is
/// exposed.
#[cfg(target_os = "windows")]
pub use capture::WindowsCaptureSession;

/// Typed read-only capture counters and one snapshot of them (FRAME-0203).
///
/// The data path beside the human-readable capture diagnostics: real counter values from
/// a running session, never a parsed report.
#[cfg(target_os = "windows")]
pub use capture::{CaptureCounters, CaptureSessionStats};

pub use capture::WindowsMediaScheduling;
/// The diagnostic capture probe used by the read-only example (FRAME-0103).
///
/// It exercises the production session and writes no file and no recording.
#[cfg(target_os = "windows")]
pub use capture::capture_probe;

/// A Frame-owned desktop surface lease (FRAME-0103).
#[cfg(target_os = "windows")]
pub use surface::WindowsSurface;

/// One running GPU region capture session (FRAME-0205).
///
/// It records one canonical rectangle of a selected display at its real resolution by
/// copying the completed capture surface on the same Direct3D 11 device into a bounded crop
/// texture pool, and it exposes those crops as the accepted [`WindowsSurface`] leases. No
/// CPU roundtrip, no scaling and no clamping happen on this path.
#[cfg(target_os = "windows")]
pub use region::RegionCaptureSession;

/// One running hardware H.264 encoder session (FRAME-0104).
///
/// It consumes [`WindowsSurface`] leases through the canonical
/// `frame_encode::PacketEncoder` contract and exposes no COM object, SDK type, raw
/// pointer or worker queue.
#[cfg(target_os = "windows")]
pub use encode::WindowsH264Encoder;

/// Runs the isolated MF encoder protocol in this executable before desktop startup.
/// Called only for the private `--frame-mf-helper` child process.
#[cfg(target_os = "windows")]
pub fn run_mf_helper() -> i32 {
    encode::mf_helper_entry()
}

#[cfg(target_os = "windows")]
pub fn run_wgc_helper() -> i32 {
    window_capture::wgc_helper_entry()
}

/// Diagnostic-only GPU shared-texture proof. Does not start WGC capture.
#[cfg(target_os = "windows")]
#[doc(hidden)]
pub fn run_shared_texture_proof_child() -> i32 {
    encode::shared_texture_child_entry()
}

/// Diagnostic-only parent proof; the caller muxes its one encoded packet.
#[cfg(target_os = "windows")]
#[doc(hidden)]
pub fn synthetic_shared_texture_h264_frame() -> Result<
    (
        frame_encode::EncodedStreamInfo,
        frame_encode::EncodedPacket,
        String,
    ),
    Box<dyn std::error::Error>,
> {
    encode::shared_texture_parent_proof()
}

/// Diagnostic-only child stall/termination proof; no capture or encode is started.
#[cfg(target_os = "windows")]
#[doc(hidden)]
pub fn synthetic_shared_texture_watchdog_proof() -> Result<String, Box<dyn std::error::Error>> {
    encode::shared_texture_watchdog_proof()
}

/// The explicitly reported encoder configuration (FRAME-0104).
#[cfg(target_os = "windows")]
pub use encode::NativeEncoderConfig;

/// The configuration a native encoder session actually reported (FRAME-0104).
#[cfg(target_os = "windows")]
pub use encode::NativeEncoderSettings;
#[cfg(target_os = "windows")]
pub use encode::NativeVideoProfile;
#[cfg(target_os = "windows")]
pub use encode::probe_mf_av1_for_adapter;
#[cfg(target_os = "windows")]
pub use encode::probe_native_profile_for_adapter;
#[cfg(target_os = "windows")]
pub use encode::probe_native_profiles;

/// The rate-control mode, preset/tuning pair and quality settings a session is
/// initialized with (FRAME-0207). The caller's policy decides them; the encoder applies
/// exactly what was decided and reports it back.
#[cfg(target_os = "windows")]
pub use encode::{
    NativeEncoderPreset, NativeQpStatistics, NativeQualitySettings, NativeRateControl,
    NativeRateControlSupport,
};

/// One qualified hardware encoder candidate (FRAME-0104).
#[cfg(target_os = "windows")]
pub use encode::NativeEncoderCandidate;
#[cfg(target_os = "windows")]
pub use encode::{
    MfH264Preparation, NativeAdapterIdentity, capture_adapter_identity, enumerate_native_adapters,
    prepare_hardware_h264, prepare_media_foundation_h264_for_surface,
    probe_native_encoders_for_surface,
};
pub use encode::{NativeAv1Candidate, probe_native_av1, probe_native_av1_diagnostics};
pub use encode::{NativeHevcMain10Candidate, probe_native_hevc_main10};

/// A snapshot of the encoder's bounded delivery counters (FRAME-0104).
#[cfg(target_os = "windows")]
pub use encode::EncoderCounterSnapshot;

/// Typed read-only encoder counters and gauges, with the checked packet/frame equality
/// (FRAME-0203).
#[cfg(target_os = "windows")]
pub use encode::EncoderSessionStats;
#[cfg(target_os = "windows")]
pub use encode::{MfStageTiming, MfTransferFormat, MfTransferStats};

#[cfg(target_os = "windows")]
pub use encode::adapter_driver_version;
#[cfg(target_os = "windows")]
pub use encode::display_capture_adapter_identity;
#[cfg(target_os = "windows")]
pub use encode::probe_h264_encoder_for_adapter;
/// Discovers and qualifies the local NVIDIA hardware encoders (FRAME-0104).
#[cfg(target_os = "windows")]
pub use encode::probe_native_encoders;
#[cfg(target_os = "windows")]
pub use encode::synthetic_media_foundation_h264_frame;
#[cfg(target_os = "windows")]
pub use encode::synthetic_media_foundation_h264_sequence;
#[cfg(target_os = "windows")]
pub use encode::{
    SyntheticEncoderSequence, synthetic_encoder_sequence,
    synthetic_encoder_sequence_with_pattern_period,
};

/// Human-readable encoder qualification diagnostics (FRAME-0104).
#[cfg(target_os = "windows")]
pub use encode::encode_diagnostics;

/// The proof limits and qualification configuration the encoder example reports
/// (FRAME-0104). They are public so a probe can report the same bounds the encoder
/// enforces instead of restating them.
#[cfg(target_os = "windows")]
pub use encode::{
    MAX_ACCESS_UNIT_BYTES, MAX_CODEC_CONFIG_BYTES, MAX_PACKET_BYTES, MAX_PACKETS,
    PROOF_BITRATE_BPS, PROOF_GOP_FRAMES, QUALIFICATION_HEIGHT, QUALIFICATION_RATE_DEN,
    QUALIFICATION_RATE_NUM, QUALIFICATION_WIDTH,
};

/// The exclusive proof-artifact writer used by the encoder example (FRAME-0104).
///
/// Diagnostic proof tooling, not a recorder and not a container writer: it creates one
/// Annex-B stream and one JSONL sidecar, never overwrites evidence, and reports an
/// incomplete artifact instead of hiding a write failure.
#[cfg(target_os = "windows")]
pub use encode::ProofArtifacts;

pub struct PlatformBackend;

/// Read-only display metadata for native shell/workbench views. Does not open
/// capture, enumerate audio, or schedule encoder calibration.
#[cfg(target_os = "windows")]
pub fn display_sources() -> Result<Vec<DisplayInfo>, RecorderError> {
    display::enumerate_displays()
}

impl frame_capture::DisplayBackend for PlatformBackend {
    #[cfg(target_os = "windows")]
    fn enumerate_displays(&self) -> Result<Vec<DisplayInfo>, RecorderError> {
        display::enumerate_displays()
    }
    #[cfg(not(target_os = "windows"))]
    fn enumerate_displays(&self) -> Result<Vec<DisplayInfo>, RecorderError> {
        Err(RecorderError::unsupported(
            "display enumeration is implemented for Windows only",
        ))
    }
    fn enumerate_windows(&self) -> Result<Vec<WindowInfo>, RecorderError> {
        #[cfg(target_os = "windows")]
        {
            window_discovery::enumerate_windows()
        }
        #[cfg(not(target_os = "windows"))]
        {
            Err(RecorderError::unsupported("windows window enumeration"))
        }
    }
}
/// One running Windows microphone session (FRAME-0105).
///
/// It exposes the canonical PCM session contract and human-readable diagnostics only:
/// no COM object, native handle, SDK type, raw pointer or worker queue is public.
#[cfg(target_os = "windows")]
pub use microphone::WindowsMicrophoneSession;

/// Human-readable microphone enumeration diagnostics (FRAME-0105).
///
/// Diagnostic text for the read-only probe, not a data API.
#[cfg(target_os = "windows")]
pub use microphone::microphone_diagnostics;
#[cfg(target_os = "windows")]
pub use microphone::{MicrophoneFormatCapability, probe_microphone_format};

/// Read-only Windows application-session discovery and bounded diagnostic evidence.
/// These snapshots expose canonical sources; they do not capture process audio.
#[cfg(target_os = "windows")]
pub use application_sessions::{
    APPLICATION_SESSION_LIMITS, ApplicationEndpointIssue, ApplicationEndpointRecord,
    ApplicationExecutableAvailability, ApplicationProcessAvailability, ApplicationSessionCounters,
    ApplicationSessionLimits, ApplicationSessionReadFailure, ApplicationSessionRecord,
    ApplicationSessionSkipReason, ApplicationSessionSnapshot, application_audio_diagnostics,
    application_audio_session_snapshot, enumerate_application_audio_sources,
};

impl frame_audio::AudioBackend for PlatformBackend {
    /// Microphone endpoints and application sessions, sorted by their full identities.
    /// Each discovery pass must succeed; an error never becomes a partial source list.
    /// Session discovery does not enable process capture or change IPC capabilities.
    #[cfg(target_os = "windows")]
    fn enumerate_audio_sources(&self) -> Result<Vec<AudioSource>, RecorderError> {
        let mut sources = microphone::enumerate_microphones()?;
        sources.extend(system_audio::enumerate()?);
        sources.extend(application_sessions::enumerate_application_audio_sources()?);
        audio_identity::enrich(&mut sources)?;
        system_audio::append_complements(&mut sources)?;
        sources.sort_unstable_by(|a, b| a.id.cmp(&b.id));
        Ok(sources)
    }
    #[cfg(not(target_os = "windows"))]
    fn enumerate_audio_sources(&self) -> Result<Vec<AudioSource>, RecorderError> {
        Err(RecorderError::unsupported("windows audio enumeration"))
    }
}
impl frame_encode::EncoderProbe for PlatformBackend {
    #[cfg(target_os = "windows")]
    fn probe(&self) -> Result<Vec<frame_encode::EncoderCapability>, RecorderError> {
        encode::probe_native_profiles().map(|candidates| {
            candidates
                .into_iter()
                .map(|candidate| candidate.capability)
                .collect()
        })
    }
    #[cfg(not(target_os = "windows"))]
    fn probe(&self) -> Result<Vec<frame_encode::EncoderCapability>, RecorderError> {
        Err(RecorderError::unsupported("windows encoder probing"))
    }
}
