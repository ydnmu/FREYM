use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::num::{NonZeroU16, NonZeroU32, NonZeroU64};
use std::ptr::{self, NonNull};
use std::sync::Arc;

use frame_audio::{
    AudioBackend, AudioCaptureBackend, AudioContext, AudioSession, DrainableAudioSession, PcmBlock,
    PcmDrainPoll,
};
use frame_capture::{
    CaptureBackend, CaptureContext, CaptureSession, CapturedFrame, DisplayBackend,
};
use frame_clock::{ClockAnchor, MasterClock, MediaTimestamp};
use frame_contracts::{
    AudioRoute, AudioSource, AudioSourceId, AudioSourceKind, CaptureTarget, DisplayId, DisplayInfo,
    ErrorCode, FpsMode, RecorderError, RecoveryStatus, WindowId, WindowInfo,
};
use frame_encode::{
    BitstreamFormat, EncodedPacket, EncodedStreamInfo, EncoderCapability, EncoderInput,
    EncoderPoll, EncoderProbe, PacketEncoder, SubmitOutcome,
};
use serde::Deserialize;

pub const MINIMUM_MACOS_VERSION: &str = "14.0";
pub const MAX_CAPTURE_SURFACES: usize = 4;
pub const MAX_ENCODER_IN_FLIGHT: usize = 8;
pub const MAX_ENCODER_PACKETS: usize = 8;
pub const MAX_AUDIO_BLOCKS: usize = 32;

#[repr(C)]
struct NativeFrame {
    surface: *mut c_void,
    pts_ns: u64,
    width: u32,
    height: u32,
    pixel_format: u32,
}
#[repr(C)]
struct NativePacket {
    data: *mut c_void,
    length: usize,
    pts_ns: u64,
    dts_ns: u64,
    duration_ns: u64,
    keyframe: c_int,
}
#[repr(C)]
struct NativeAudioBlock {
    data: *mut c_void,
    count: usize,
    pts_ns: u64,
    rate: u32,
    channels: u16,
}

unsafe extern "C" {
    fn fm_screen_authorized() -> c_int;
    fn fm_host_time_ns() -> u64;
    fn fm_discover(error: *mut *mut c_char) -> *mut c_char;
    fn fm_free(value: *mut c_void);
    fn fm_capture_start(
        kind: c_int,
        id: u32,
        width: u32,
        height: u32,
        rate_num: u32,
        rate_den: u32,
        error: *mut *mut c_char,
    ) -> *mut c_void;
    fn fm_capture_poll(
        capture: *mut c_void,
        output: *mut NativeFrame,
        error: *mut *mut c_char,
    ) -> c_int;
    fn fm_capture_stop(capture: *mut c_void, error: *mut *mut c_char) -> c_int;
    fn fm_surface_release(surface: *mut c_void);
    fn fm_encoder_start(
        width: u32,
        height: u32,
        rate_num: u32,
        rate_den: u32,
        bitrate: u32,
        peak_bitrate: u32,
        gop: u32,
        error: *mut *mut c_char,
    ) -> *mut c_void;
    fn fm_encoder_submit(
        encoder: *mut c_void,
        surface: *mut c_void,
        pts_ns: u64,
        duration_ns: u64,
        error: *mut *mut c_char,
    ) -> c_int;
    fn fm_encoder_poll(
        encoder: *mut c_void,
        output: *mut NativePacket,
        error: *mut *mut c_char,
    ) -> c_int;
    fn fm_encoder_config(encoder: *mut c_void, length: *mut usize) -> *mut c_void;
    fn fm_encoder_drain(encoder: *mut c_void, error: *mut *mut c_char) -> c_int;
    fn fm_encoder_drained(encoder: *mut c_void) -> c_int;
    fn fm_encoder_stop(encoder: *mut c_void);
    fn fm_microphone_start(device: *const c_char, error: *mut *mut c_char) -> *mut c_void;
    fn fm_application_audio_start(pid: u32, display: u32, error: *mut *mut c_char) -> *mut c_void;
    fn fm_audio_poll(
        audio: *mut c_void,
        output: *mut NativeAudioBlock,
        error: *mut *mut c_char,
    ) -> c_int;
    fn fm_audio_request_drain(audio: *mut c_void, error: *mut *mut c_char) -> c_int;
    fn fm_audio_stop(audio: *mut c_void, error: *mut *mut c_char) -> c_int;
}

fn error(code: ErrorCode, message: impl Into<String>, action: &str) -> RecorderError {
    RecorderError {
        code,
        message: message.into(),
        recovery: RecoveryStatus::NoRecording,
        action: action.into(),
    }
}
fn native_error(raw: *mut c_char, code: ErrorCode) -> RecorderError {
    let message = if raw.is_null() {
        "Apple media API returned no error detail.".into()
    } else {
        // SAFETY: the bridge returns an owned, NUL-terminated strdup string.
        let value = unsafe { CStr::from_ptr(raw).to_string_lossy().into_owned() };
        unsafe { fm_free(raw.cast()) };
        value
    };
    error(
        code,
        message,
        "Review macOS Privacy & Security permissions and refresh the source list.",
    )
}
fn anchor(clock: &Arc<MasterClock>) -> Result<ClockAnchor, RecorderError> {
    let before = clock.now();
    let native = unsafe { fm_host_time_ns() };
    let after = clock.now();
    ClockAnchor::from_sample(
        native,
        NonZeroU64::new(1_000_000_000).unwrap(),
        before,
        after,
    )
    .ok_or_else(|| {
        error(
            ErrorCode::Internal,
            "Could not align the host clock to the recording clock.",
            "Restart recording.",
        )
    })
}
fn map_time(anchor: ClockAnchor, native: u64) -> Result<MediaTimestamp, RecorderError> {
    anchor.map(native).ok_or_else(|| {
        error(
            ErrorCode::Internal,
            "Native timestamp is outside this recording's clock.",
            "Restart recording.",
        )
    })
}

#[derive(Deserialize)]
struct Discovery {
    #[serde(rename = "screenAuthorized")]
    screen_authorized: bool,
    displays: Vec<NativeDisplay>,
    windows: Vec<NativeWindow>,
    applications: Vec<NativeApp>,
    microphones: Vec<NativeMicrophone>,
}
#[derive(Deserialize)]
struct NativeDisplay {
    id: u32,
    name: String,
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    scale: f64,
    rate: Option<f64>,
    primary: bool,
}
#[derive(Deserialize)]
struct NativeWindow {
    id: u32,
    title: String,
    pid: u32,
    width: u32,
    height: u32,
}
#[derive(Deserialize)]
struct NativeApp {
    pid: u32,
    bundle: String,
    name: String,
}
#[derive(Deserialize)]
struct NativeMicrophone {
    id: String,
    name: String,
}
fn discover() -> Result<Discovery, RecorderError> {
    let mut raw_error = ptr::null_mut();
    let raw = unsafe { fm_discover(&mut raw_error) };
    if raw.is_null() {
        return Err(native_error(raw_error, ErrorCode::SourceLost));
    }
    let json = unsafe { CStr::from_ptr(raw).to_string_lossy().into_owned() };
    unsafe { fm_free(raw.cast()) };
    serde_json::from_str(&json).map_err(|err| {
        error(
            ErrorCode::Internal,
            format!("Native source discovery returned invalid data: {err}"),
            "Refresh sources.",
        )
    })
}
pub fn screen_authorized() -> bool {
    unsafe { fm_screen_authorized() != 0 }
}

#[derive(Clone, Copy, Default)]
pub struct PlatformBackend {
    pub application_display_id: Option<u32>,
}
impl DisplayBackend for PlatformBackend {
    fn enumerate_displays(&self) -> Result<Vec<DisplayInfo>, RecorderError> {
        Ok(discover()?
            .displays
            .into_iter()
            .map(|d| DisplayInfo {
                id: DisplayId(d.id.to_string()),
                name: d.name,
                width: d.width,
                height: d.height,
                x: d.x,
                y: d.y,
                scale_factor: d.scale,
                refresh_rate_hz: d.rate,
                is_primary: d.primary,
                hdr_available: false,
                required_recording_quality: None,
            })
            .collect())
    }
    fn enumerate_windows(&self) -> Result<Vec<WindowInfo>, RecorderError> {
        let discovery = discover()?;
        if !discovery.screen_authorized {
            return Err(error(
                ErrorCode::PermissionDenied,
                "Screen recording permission is required to list windows.",
                "Press Record for a display to request permission, then refresh sources.",
            ));
        }
        Ok(discovery
            .windows
            .into_iter()
            .map(|w| WindowInfo {
                id: WindowId(w.id.to_string()),
                title: w.title,
                process_id: NonZeroU32::new(w.pid).map(NonZeroU32::get),
                process_name: None,
            })
            .collect())
    }
}
impl AudioBackend for PlatformBackend {
    fn enumerate_audio_sources(&self) -> Result<Vec<AudioSource>, RecorderError> {
        let content = discover()?;
        let mut sources: Vec<AudioSource> = content
            .microphones
            .into_iter()
            .map(|device| AudioSource {
                controls: Default::default(),
                capture_identity: None,
                muted: false,
                id: AudioSourceId(format!("mic:{}", device.id)),
                kind: AudioSourceKind::Microphone,
                display_name: device.name,
                process_id: None,
                app_identifier: None,
                is_active: true,
                route: AudioRoute::Ignore,
            })
            .collect();
        if content.screen_authorized {
            sources.extend(content.applications.into_iter().map(|app| AudioSource {
                controls: Default::default(),
                capture_identity: None,
                muted: false,
                id: AudioSourceId(format!("app:{}:{}", app.pid, app.bundle)),
                kind: AudioSourceKind::Application,
                display_name: app.name,
                process_id: Some(app.pid),
                app_identifier: Some(app.bundle),
                is_active: true,
                route: AudioRoute::Ignore,
            }));
        }
        Ok(sources)
    }
}
impl EncoderProbe for PlatformBackend {
    fn probe(&self) -> Result<Vec<EncoderCapability>, RecorderError> {
        let config = MacEncoderConfig {
            width: 640,
            height: 480,
            rate_num: NonZeroU32::new(30).unwrap(),
            rate_den: NonZeroU32::new(1).unwrap(),
            bitrate_bps: 1_000_000,
            peak_bitrate_bps: 2_000_000,
            gop_frames: 60,
        };
        let encoder = MacH264Encoder::open(config)?;
        Ok(vec![encoder.stream_info().capability.clone()])
    }
}
pub fn probe_encoder(config: MacEncoderConfig) -> Result<EncoderCapability, RecorderError> {
    let encoder = MacH264Encoder::open(config)?;
    Ok(encoder.stream_info().capability.clone())
}

/// A retained CVPixelBuffer. The capture callback owns at most four until dequeued;
/// this owner releases exactly the buffer transferred to Rust.
pub struct MacSurface {
    raw: NonNull<c_void>,
    width: u32,
    height: u32,
    pixel_format: u32,
}
impl MacSurface {
    fn as_ptr(&self) -> *mut c_void {
        self.raw.as_ptr()
    }
}
impl Drop for MacSurface {
    fn drop(&mut self) {
        unsafe { fm_surface_release(self.as_ptr()) }
    }
}

pub struct MacCaptureSession {
    raw: Option<NonNull<c_void>>,
    anchor: ClockAnchor,
    cancellation: frame_capture::CaptureCancellation,
    geometry: (u32, u32),
}
impl CaptureBackend for PlatformBackend {
    type Session = MacCaptureSession;
    fn start(
        &mut self,
        target: &CaptureTarget,
        fps: FpsMode,
        context: CaptureContext,
    ) -> Result<Self::Session, RecorderError> {
        let (num, den) = match fps {
            FpsMode::Fixed(value) if value.is_finite() && (1.0..=120.0).contains(&value) => {
                (value.round() as u32, 1)
            }
            FpsMode::AutoMatchSource => (60, 1),
            _ => return Err(RecorderError::invalid("Invalid capture frame rate.")),
        };
        self.start_with_rate(target, num, den, context)
    }
}
impl PlatformBackend {
    pub fn start_with_rate(
        &mut self,
        target: &CaptureTarget,
        rate_num: u32,
        rate_den: u32,
        context: CaptureContext,
    ) -> Result<MacCaptureSession, RecorderError> {
        let (kind, id, width, height) = match target {
            CaptureTarget::Display(id) => {
                let value: u32 =
                    id.0.parse()
                        .map_err(|_| RecorderError::invalid("Invalid macOS display ID."))?;
                let d = discover()?
                    .displays
                    .into_iter()
                    .find(|d| d.id == value)
                    .ok_or_else(|| {
                        error(
                            ErrorCode::SourceLost,
                            "Selected display disappeared.",
                            "Refresh sources.",
                        )
                    })?;
                (0, value, d.width, d.height)
            }
            CaptureTarget::Window(id) => {
                let value: u32 =
                    id.0.parse()
                        .map_err(|_| RecorderError::invalid("Invalid macOS window ID."))?;
                let w = discover()?
                    .windows
                    .into_iter()
                    .find(|w| w.id == value)
                    .ok_or_else(|| {
                        error(
                            ErrorCode::SourceLost,
                            "Selected window disappeared.",
                            "Refresh sources.",
                        )
                    })?;
                (1, value, w.width, w.height)
            }
            _ => return Err(RecorderError::unsupported("macOS region and game capture")),
        };
        if width == 0 || height == 0 {
            return Err(RecorderError::invalid(
                "Capture target has zero dimensions.",
            ));
        }
        if rate_num == 0 || rate_num > i32::MAX as u32 || rate_den == 0 {
            return Err(RecorderError::invalid("Invalid capture frame rate."));
        }
        let mapped = anchor(&context.clock)?;
        let mut raw_error = ptr::null_mut();
        let raw = unsafe {
            fm_capture_start(kind, id, width, height, rate_num, rate_den, &mut raw_error)
        };
        let raw = NonNull::new(raw)
            .ok_or_else(|| native_error(raw_error, ErrorCode::PermissionDenied))?;
        Ok(MacCaptureSession {
            raw: Some(raw),
            anchor: mapped,
            cancellation: context.cancellation,
            geometry: (width, height),
        })
    }
}
pub fn capture_dimensions(target: &CaptureTarget) -> Result<(u32, u32), RecorderError> {
    let content = discover()?;
    match target {
        CaptureTarget::Display(id) => content
            .displays
            .iter()
            .find(|d| d.id.to_string() == id.0)
            .map(|d| (d.width, d.height)),
        CaptureTarget::Window(id) => content
            .windows
            .iter()
            .find(|w| w.id.to_string() == id.0)
            .map(|w| (w.width, w.height)),
        _ => None,
    }
    .ok_or_else(|| {
        error(
            ErrorCode::SourceLost,
            "Selected capture target disappeared.",
            "Refresh sources.",
        )
    })
}
impl CaptureSession for MacCaptureSession {
    type Surface = MacSurface;
    fn try_frame(&mut self) -> Result<Option<CapturedFrame<Self::Surface>>, RecorderError> {
        if self.cancellation.is_cancelled() {
            return Ok(None);
        }
        let Some(raw) = self.raw else {
            return Ok(None);
        };
        let mut frame = NativeFrame {
            surface: ptr::null_mut(),
            pts_ns: 0,
            width: 0,
            height: 0,
            pixel_format: 0,
        };
        let mut raw_error = ptr::null_mut();
        match unsafe { fm_capture_poll(raw.as_ptr(), &mut frame, &mut raw_error) } {
            0 => Ok(None),
            1 => {
                let surface = MacSurface {
                    raw: NonNull::new(frame.surface).ok_or_else(|| {
                        error(
                            ErrorCode::Internal,
                            "Capture returned no surface.",
                            "Restart recording.",
                        )
                    })?,
                    width: frame.width,
                    height: frame.height,
                    pixel_format: frame.pixel_format,
                };
                crate::media_contract::surface(
                    self.geometry,
                    (frame.width, frame.height),
                    frame.pixel_format,
                )?;
                Ok(Some(CapturedFrame {
                    surface,
                    timestamp: map_time(self.anchor, frame.pts_ns)?,
                    width: frame.width,
                    height: frame.height,
                }))
            }
            _ => Err(native_error(raw_error, ErrorCode::SourceLost)),
        }
    }
    fn stop(&mut self) -> Result<(), RecorderError> {
        self.cancellation.cancel();
        let Some(raw) = self.raw.take() else {
            return Ok(());
        };
        let mut raw_error = ptr::null_mut();
        if unsafe { fm_capture_stop(raw.as_ptr(), &mut raw_error) } == 0 {
            Ok(())
        } else {
            Err(native_error(raw_error, ErrorCode::SourceLost))
        }
    }
}
impl Drop for MacCaptureSession {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

pub struct MacAudioSession {
    raw: Option<NonNull<c_void>>,
    source: AudioSourceId,
    anchor: ClockAnchor,
    cancellation: frame_audio::AudioCancellation,
    draining: bool,
    channels: NonZeroU16,
    ready_pcm: Option<PcmBlock>,
}
impl AudioCaptureBackend for PlatformBackend {
    type Session = MacAudioSession;
    fn start_audio(
        &mut self,
        source: &AudioSourceId,
        context: AudioContext,
    ) -> Result<Self::Session, RecorderError> {
        let mapped = anchor(&context.clock)?;
        let mut raw_error = ptr::null_mut();
        let raw = if let Some(id) = source.0.strip_prefix("mic:") {
            let id =
                CString::new(id).map_err(|_| RecorderError::invalid("Invalid microphone ID."))?;
            unsafe { fm_microphone_start(id.as_ptr(), &mut raw_error) }
        } else if let Some(rest) = source.0.strip_prefix("app:") {
            let (pid, bundle) = rest
                .split_once(':')
                .ok_or_else(|| RecorderError::invalid("Invalid application audio ID."))?;
            let pid: u32 = pid
                .parse()
                .map_err(|_| RecorderError::invalid("Invalid application process ID."))?;
            let content = discover()?;
            if !content
                .applications
                .iter()
                .any(|app| app.pid == pid && app.bundle == bundle)
            {
                return Err(error(
                    ErrorCode::SourceLost,
                    "Selected application audio source disappeared.",
                    "Refresh sources.",
                ));
            }
            let display = self
                .application_display_id
                .or_else(|| content.displays.first().map(|d| d.id))
                .ok_or_else(|| {
                    error(
                        ErrorCode::SourceLost,
                        "No display is available for application audio filtering.",
                        "Connect a display.",
                    )
                })?;
            unsafe { fm_application_audio_start(pid, display, &mut raw_error) }
        } else {
            return Err(RecorderError::invalid("Unknown macOS audio source ID."));
        };
        let raw = NonNull::new(raw)
            .ok_or_else(|| native_error(raw_error, ErrorCode::PermissionDenied))?;
        let mut session = MacAudioSession {
            raw: Some(raw),
            source: source.clone(),
            anchor: mapped,
            cancellation: context.cancellation,
            draining: false,
            channels: NonZeroU16::new(2).unwrap(),
            ready_pcm: None,
        };
        // ScreenCaptureKit explicitly negotiates stereo. A quiet application must
        // not need to produce its first sound before recording can start.
        if source.0.starts_with("app:") {
            return Ok(session);
        }
        // Freeze the negotiated layout before Opus/MKV headers are created. Keep
        // the readiness sample, including its native timestamp, for the first poll.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if session.cancellation.is_cancelled() || std::time::Instant::now() >= deadline {
                session.stop()?;
                return Err(error(
                    ErrorCode::SourceLost,
                    "Audio layout readiness timed out or was cancelled.",
                    "Refresh audio sources.",
                ));
            }
            match session.try_pcm() {
                Ok(Some(block)) => {
                    if !matches!(block.channels.get(), 1 | 2) {
                        session.stop()?;
                        return Err(RecorderError::unsupported("Native audio channel layout"));
                    }
                    session.channels = block.channels;
                    session.ready_pcm = Some(block);
                    return Ok(session);
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(2)),
                Err(error) => {
                    session.stop()?;
                    return Err(error);
                }
            }
        }
    }
}
impl AudioSession for MacAudioSession {
    fn pcm_channels(&self) -> NonZeroU16 {
        self.channels
    }
    fn try_pcm(&mut self) -> Result<Option<PcmBlock>, RecorderError> {
        if let Some(block) = self.ready_pcm.take() {
            return Ok(Some(block));
        }
        let Some(raw) = self.raw else {
            return Ok(None);
        };
        let mut block = NativeAudioBlock {
            data: ptr::null_mut(),
            count: 0,
            pts_ns: 0,
            rate: 0,
            channels: 0,
        };
        let mut raw_error = ptr::null_mut();
        match unsafe { fm_audio_poll(raw.as_ptr(), &mut block, &mut raw_error) } {
            0 => Ok(None),
            1 => {
                if block.data.is_null() || block.count > 32 * 1024 * 8 || block.count == 0 {
                    if !block.data.is_null() {
                        unsafe { fm_free(block.data) }
                    }
                    return Err(error(
                        ErrorCode::Internal,
                        "Invalid native PCM block size.",
                        "Restart recording.",
                    ));
                }
                let samples = unsafe {
                    std::slice::from_raw_parts(block.data.cast::<f32>(), block.count).to_vec()
                };
                unsafe { fm_free(block.data) };
                Ok(Some(PcmBlock {
                    source_id: self.source.clone(),
                    timestamp: map_time(self.anchor, block.pts_ns)?,
                    sample_rate: NonZeroU32::new(block.rate)
                        .ok_or_else(|| RecorderError::invalid("Native PCM rate is zero."))?,
                    channels: NonZeroU16::new(block.channels).ok_or_else(|| {
                        RecorderError::invalid("Native PCM channel count is zero.")
                    })?,
                    samples,
                }))
            }
            _ => Err(native_error(raw_error, ErrorCode::SourceLost)),
        }
    }
    fn stop(&mut self) -> Result<(), RecorderError> {
        self.cancellation.cancel();
        self.ready_pcm = None;
        let Some(raw) = self.raw.take() else {
            return Ok(());
        };
        let mut raw_error = ptr::null_mut();
        if unsafe { fm_audio_stop(raw.as_ptr(), &mut raw_error) } == 0 {
            Ok(())
        } else {
            Err(native_error(raw_error, ErrorCode::SourceLost))
        }
    }
}
impl DrainableAudioSession for MacAudioSession {
    fn request_drain(&mut self) -> Result<(), RecorderError> {
        if !self.draining {
            let Some(raw) = self.raw else {
                return Err(RecorderError::invalid("Audio session is stopped."));
            };
            let mut raw_error = ptr::null_mut();
            if unsafe { fm_audio_request_drain(raw.as_ptr(), &mut raw_error) } != 0 {
                return Err(native_error(raw_error, ErrorCode::SourceLost));
            }
            self.draining = true;
        }
        Ok(())
    }
    fn poll_drain(&mut self) -> Result<PcmDrainPoll, RecorderError> {
        if !self.draining {
            return Err(RecorderError::invalid("Audio drain was not requested."));
        }
        if let Some(block) = self.try_pcm()? {
            return Ok(PcmDrainPoll::Block(block));
        }
        self.stop()?;
        Ok(PcmDrainPoll::Drained)
    }
}
impl Drop for MacAudioSession {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[derive(Clone, Copy)]
pub struct MacEncoderConfig {
    pub width: u32,
    pub height: u32,
    pub rate_num: NonZeroU32,
    pub rate_den: NonZeroU32,
    pub bitrate_bps: u32,
    pub peak_bitrate_bps: u32,
    pub gop_frames: u32,
}
pub struct MacH264Encoder {
    raw: Option<NonNull<c_void>>,
    info: EncodedStreamInfo,
    draining: bool,
    drained: bool,
    accepted: u64,
    completed: u64,
    last_submitted: Option<u64>,
    last_completed: Option<u64>,
    failure: Option<RecorderError>,
}
impl MacH264Encoder {
    pub fn open(config: MacEncoderConfig) -> Result<Self, RecorderError> {
        let width = NonZeroU32::new(config.width)
            .ok_or_else(|| RecorderError::invalid("Zero encoder width."))?;
        let height = NonZeroU32::new(config.height)
            .ok_or_else(|| RecorderError::invalid("Zero encoder height."))?;
        if config.width > i32::MAX as u32
            || config.height > i32::MAX as u32
            || config.bitrate_bps == 0
            || config.peak_bitrate_bps < config.bitrate_bps
            || config.gop_frames == 0
        {
            return Err(RecorderError::invalid(
                "Invalid fixed VideoToolbox configuration.",
            ));
        }
        let mut raw_error = ptr::null_mut();
        let raw = unsafe {
            fm_encoder_start(
                config.width,
                config.height,
                config.rate_num.get(),
                config.rate_den.get(),
                config.bitrate_bps,
                config.peak_bitrate_bps,
                config.gop_frames,
                &mut raw_error,
            )
        };
        let raw = NonNull::new(raw)
            .ok_or_else(|| native_error(raw_error, ErrorCode::EncoderUnavailable))?;
        Ok(Self {
            raw: Some(raw),
            info: EncodedStreamInfo {
                capability: EncoderCapability {
                    id: "macos-videotoolbox-h264".into(),
                    codec: "h264".into(),
                    hardware: true,
                },
                width,
                height,
                nominal_rate_num: config.rate_num,
                nominal_rate_den: config.rate_den,
                bitstream_format: BitstreamFormat::H264AnnexB,
                colour: None,
                codec_config: Vec::new(),
                allows_reordering: false,
            },
            draining: false,
            drained: false,
            accepted: 0,
            completed: 0,
            last_submitted: None,
            last_completed: None,
            failure: None,
        })
    }
    pub fn refresh_config(&mut self) -> bool {
        if self.failure.is_some() {
            return false;
        }
        let Some(raw) = self.raw else {
            return false;
        };
        let mut length = 0;
        let bytes = unsafe { fm_encoder_config(raw.as_ptr(), &mut length) };
        if bytes.is_null() {
            return false;
        }
        if length == 0 || length > crate::media_contract::MAX_CODEC_CONFIG {
            unsafe { fm_free(bytes) };
            self.failure = Some(RecorderError::invalid(
                "Invalid VideoToolbox H.264 configuration size.",
            ));
            return false;
        }
        let observed = unsafe { std::slice::from_raw_parts(bytes.cast::<u8>(), length) };
        let checked = crate::media_contract::codec_config(&self.info.codec_config, observed);
        if checked.is_ok() && self.info.codec_config.is_empty() {
            self.info.codec_config = observed.to_vec();
        }
        unsafe { fm_free(bytes) };
        if let Err(error) = checked {
            self.failure = Some(error);
            false
        } else {
            true
        }
    }
    pub fn counts(&self) -> (u64, u64) {
        (self.accepted, self.completed)
    }
}
impl PacketEncoder<EncoderInput<CapturedFrame<MacSurface>>> for MacH264Encoder {
    fn submit(
        &mut self,
        input: EncoderInput<CapturedFrame<MacSurface>>,
    ) -> Result<SubmitOutcome<EncoderInput<CapturedFrame<MacSurface>>>, RecorderError> {
        if let Some(error) = self.failure.as_ref() {
            return Err(error.clone());
        }
        let checked = crate::media_contract::surface(
            (self.info.width.get(), self.info.height.get()),
            (input.input.surface.width, input.input.surface.height),
            input.input.surface.pixel_format,
        )
        .and_then(|()| {
            if input.input.width != input.input.surface.width
                || input.input.height != input.input.surface.height
            {
                return Err(RecorderError::invalid(
                    "The frame geometry differs from its retained surface.",
                ));
            }
            crate::media_contract::timestamp(
                self.last_submitted,
                input.input.timestamp.0,
                input.duration_ns.get(),
            )
        });
        if let Err(error) = checked {
            self.failure = Some(error.clone());
            return Err(error);
        }
        let raw = self
            .raw
            .ok_or_else(|| RecorderError::invalid("Encoder already stopped."))?;
        let mut raw_error = ptr::null_mut();
        match unsafe {
            fm_encoder_submit(
                raw.as_ptr(),
                input.input.surface.as_ptr(),
                input.input.timestamp.0,
                input.duration_ns.get(),
                &mut raw_error,
            )
        } {
            1 => {
                self.accepted += 1;
                self.last_submitted = Some(input.input.timestamp.0);
                Ok(SubmitOutcome::Accepted)
            }
            0 => Ok(SubmitOutcome::Backpressure(input)),
            _ => Err(native_error(raw_error, ErrorCode::EncoderUnavailable)),
        }
    }
    fn try_packet(&mut self) -> Result<EncoderPoll, RecorderError> {
        if let Some(error) = self.failure.as_ref() {
            return Err(error.clone());
        }
        if self.drained {
            return Ok(EncoderPoll::Drained);
        }
        let raw = self
            .raw
            .ok_or_else(|| RecorderError::invalid("Encoder already stopped."))?;
        let mut packet = NativePacket {
            data: ptr::null_mut(),
            length: 0,
            pts_ns: 0,
            dts_ns: 0,
            duration_ns: 0,
            keyframe: 0,
        };
        let mut raw_error = ptr::null_mut();
        match unsafe { fm_encoder_poll(raw.as_ptr(), &mut packet, &mut raw_error) } {
            1 => {
                if packet.data.is_null()
                    || packet.length == 0
                    || packet.length > 8 * 1024 * 1024
                    || packet.pts_ns == u64::MAX
                    || packet.dts_ns == u64::MAX
                    || packet.duration_ns == u64::MAX
                    || packet.duration_ns == 0
                {
                    if !packet.data.is_null() {
                        unsafe { fm_free(packet.data) }
                    }
                    return Err(error(
                        ErrorCode::EncoderUnavailable,
                        "Invalid VideoToolbox output packet.",
                        "Restart recording.",
                    ));
                }
                let data = unsafe {
                    std::slice::from_raw_parts(packet.data.cast::<u8>(), packet.length).to_vec()
                };
                unsafe { fm_free(packet.data) };
                let checked = crate::media_contract::packet(
                    self.last_completed,
                    packet.pts_ns,
                    packet.dts_ns,
                    packet.duration_ns,
                )
                .and_then(|()| {
                    if self.completed >= self.accepted {
                        Err(RecorderError::invalid(
                            "VideoToolbox completed an unaccepted input.",
                        ))
                    } else {
                        Ok(())
                    }
                });
                if let Err(error) = checked {
                    self.failure = Some(error.clone());
                    return Err(error);
                }
                self.last_completed = Some(packet.pts_ns);
                self.completed += 1;
                Ok(EncoderPoll::Packet(EncodedPacket {
                    track_id: 0,
                    pts: MediaTimestamp(packet.pts_ns),
                    dts: MediaTimestamp(packet.dts_ns),
                    duration_ns: packet.duration_ns,
                    keyframe: packet.keyframe != 0,
                    data,
                }))
            }
            0 => {
                if unsafe { fm_encoder_drained(raw.as_ptr()) } != 0 {
                    self.drained = true;
                    Ok(EncoderPoll::Drained)
                } else {
                    Ok(EncoderPoll::Pending)
                }
            }
            _ => Err(native_error(raw_error, ErrorCode::EncoderUnavailable)),
        }
    }
    fn stream_info(&self) -> &EncodedStreamInfo {
        &self.info
    }
    fn begin_drain(&mut self) -> Result<(), RecorderError> {
        if self.draining {
            return Ok(());
        }
        let raw = self
            .raw
            .ok_or_else(|| RecorderError::invalid("Encoder already stopped."))?;
        let mut raw_error = ptr::null_mut();
        if unsafe { fm_encoder_drain(raw.as_ptr(), &mut raw_error) } != 0 {
            return Err(native_error(raw_error, ErrorCode::FinalizationFailed));
        }
        self.draining = true;
        Ok(())
    }
    fn stop(&mut self) -> Result<(), RecorderError> {
        if let Some(raw) = self.raw.take() {
            unsafe { fm_encoder_stop(raw.as_ptr()) };
        }
        if self.drained {
            Ok(())
        } else {
            Err(error(
                ErrorCode::FinalizationFailed,
                "Encoder stopped before its output drained.",
                "Discard the incomplete recording.",
            ))
        }
    }
}
impl Drop for MacH264Encoder {
    fn drop(&mut self) {
        if let Some(raw) = self.raw.take() {
            unsafe { fm_encoder_stop(raw.as_ptr()) };
        }
    }
}
