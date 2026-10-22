//! Explicit Media Foundation camera selection and bounded latest BGRA delivery.
//! Device metadata enumeration does not activate a camera. Format discovery and
//! capture activate only the caller's selected symbolic link.
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use frame_clock::{MasterClock, MediaTimestamp};
use frame_contracts::{
    CameraFormat, CameraInfo, CameraSelection, ErrorCode, RecorderError, RecoveryStatus,
};
use windows::Win32::Foundation::E_ACCESSDENIED;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::core::Interface;
use windows::core::{GUID, HRESULT, Ref, implement};

const MAX_CAMERA_BYTES: usize = 64 * 1024 * 1024;
const START_TIMEOUT: Duration = Duration::from_secs(5);
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
const VIDEO_STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

fn error(code: ErrorCode, message: impl Into<String>) -> RecorderError {
    RecorderError {
        code,
        message: message.into(),
        recovery: RecoveryStatus::NoRecording,
        action:
            "Refresh cameras, check Windows camera permission and the selected format, then retry."
                .into(),
    }
}
fn native(api: &str, failure: windows::core::Error) -> RecorderError {
    let code = if failure.code() == E_ACCESSDENIED {
        ErrorCode::PermissionDenied
    } else {
        ErrorCode::SourceLost
    };
    error(
        code,
        format!(
            "Camera {api} failed with HRESULT 0x{:08X}.",
            failure.code().0
        ),
    )
}

struct MfRuntime;
impl MfRuntime {
    fn new() -> Result<Self, RecorderError> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }
            .map_err(|e| native("COM initialization", e))?;
        if let Err(e) = unsafe { MFStartup(MF_VERSION, 0) } {
            unsafe { CoUninitialize() };
            return Err(native("Media Foundation startup", e));
        }
        Ok(Self)
    }
}
impl Drop for MfRuntime {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
}

fn attribute_string(value: &IMFActivate, key: &GUID) -> Result<String, RecorderError> {
    let len =
        unsafe { value.GetStringLength(key) }.map_err(|e| native("read device identity", e))?;
    let mut units = vec![0u16; len as usize + 1];
    unsafe { value.GetString(key, &mut units, None) }
        .map_err(|e| native("read device identity", e))?;
    Ok(String::from_utf16_lossy(&units[..len as usize]))
}

fn activations() -> Result<Vec<IMFActivate>, RecorderError> {
    let mut attrs = None;
    unsafe { MFCreateAttributes(&mut attrs, 1) }
        .map_err(|e| native("create discovery attributes", e))?;
    let attrs = attrs.ok_or_else(|| {
        error(
            ErrorCode::Internal,
            "Camera discovery attributes were not created.",
        )
    })?;
    unsafe {
        attrs.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )
    }
    .map_err(|e| native("select video devices", e))?;
    let mut raw: *mut Option<IMFActivate> = ptr::null_mut();
    let mut count = 0u32;
    unsafe { MFEnumDeviceSources(&attrs, &mut raw, &mut count) }
        .map_err(|e| native("enumerate devices", e))?;
    let mut result = Vec::with_capacity(count as usize);
    if !raw.is_null() {
        for index in 0..count as usize {
            // MFEnumDeviceSources transfers each COM reference and the outer CoTaskMem array.
            if let Some(activation) = unsafe { raw.add(index).read() } {
                result.push(activation);
            }
        }
        unsafe { CoTaskMemFree(Some(raw.cast())) };
    }
    Ok(result)
}

/// Read-only names and stable symbolic links; no device is activated.
pub fn enumerate_cameras() -> Result<Vec<CameraInfo>, RecorderError> {
    let _runtime = MfRuntime::new()?;
    let mut cameras = Vec::new();
    for activation in activations()? {
        cameras.push(CameraInfo {
            id: attribute_string(
                &activation,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
            )?,
            name: attribute_string(&activation, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)?,
        });
    }
    cameras.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(cameras)
}

fn selected_activation(id: &str) -> Result<IMFActivate, RecorderError> {
    if id.is_empty() {
        return Err(RecorderError::invalid("Select a camera before opening it."));
    }
    for activation in activations()? {
        if attribute_string(
            &activation,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
        )? == id
        {
            return Ok(activation);
        }
    }
    Err(error(
        ErrorCode::SourceLost,
        "The selected camera is no longer connected.",
    ))
}

fn format_at(
    reader: &IMFSourceReader,
    index: u32,
) -> Result<Option<(IMFMediaType, CameraFormat)>, RecorderError> {
    let native_type = match unsafe { reader.GetNativeMediaType(VIDEO_STREAM, index) } {
        Ok(value) => value,
        Err(e) if e.code() == MF_E_NO_MORE_TYPES => return Ok(None),
        Err(e) => return Err(native("read native format", e)),
    };
    let size = match unsafe { native_type.GetUINT64(&MF_MT_FRAME_SIZE) } {
        Ok(value) => value,
        Err(e) if e.code() == MF_E_ATTRIBUTENOTFOUND => 0,
        Err(e) => return Err(native("read frame size", e)),
    };
    let rate = match unsafe { native_type.GetUINT64(&MF_MT_FRAME_RATE) } {
        Ok(value) => value,
        Err(e) if e.code() == MF_E_ATTRIBUTENOTFOUND => 0,
        Err(e) => return Err(native("read frame rate", e)),
    };
    let subtype =
        unsafe { native_type.GetGUID(&MF_MT_SUBTYPE) }.map_err(|e| native("read subtype", e))?;
    let supported = (size >> 32) != 0
        && (size as u32) != 0
        && (rate >> 32) != 0
        && (rate as u32) != 0
        && (subtype == MFVideoFormat_RGB32
            || subtype == MFVideoFormat_ARGB32
            || subtype == MFVideoFormat_NV12
            || subtype == MFVideoFormat_YUY2
            || subtype == MFVideoFormat_MJPG);
    let label = if subtype == MFVideoFormat_RGB32 {
        "RGB32".to_string()
    } else if subtype == MFVideoFormat_ARGB32 {
        "ARGB32".to_string()
    } else if subtype == MFVideoFormat_NV12 {
        "NV12".to_string()
    } else if subtype == MFVideoFormat_YUY2 {
        "YUY2".to_string()
    } else if subtype == MFVideoFormat_MJPG {
        "MJPEG".to_string()
    } else {
        format!("{subtype:?}")
    };
    Ok(Some((
        native_type,
        CameraFormat {
            index,
            width: (size >> 32) as u32,
            height: size as u32,
            frame_rate_num: (rate >> 32) as u32,
            frame_rate_den: rate as u32,
            subtype: label,
            supported,
        },
    )))
}

/// Format discovery is explicit because activating the selected device may request
/// Windows camera permission. It never starts a capture worker.
pub fn enumerate_camera_formats(device_id: &str) -> Result<Vec<CameraFormat>, RecorderError> {
    let _runtime = MfRuntime::new()?;
    let activation = selected_activation(device_id)?;
    let source: IMFMediaSource = unsafe { activation.ActivateObject() }
        .map_err(|e| native("activate selected device", e))?;
    let outcome = (|| {
        let reader =
            unsafe { MFCreateSourceReaderFromMediaSource(&source, None::<&IMFAttributes>) }
                .map_err(|e| native("open format reader", e))?;
        let mut formats = Vec::new();
        for index in 0..256 {
            match format_at(&reader, index)? {
                Some((_, format)) => formats.push(format),
                None => return Ok(formats),
            }
        }
        Err(error(
            ErrorCode::InvalidRequest,
            "The camera reported more than 256 native formats.",
        ))
    })();
    unsafe {
        let _ = source.Shutdown();
    }
    outcome
}

pub struct CameraFrame {
    pub timestamp: MediaTimestamp,
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

struct Shared {
    latest: Mutex<Option<CameraFrame>>,
    terminal: OnceLock<RecorderError>,
    stopped: AtomicBool,
    delivered: AtomicU64,
}

#[implement(IMFSourceReaderCallback)]
struct ReaderCallback {
    shared: Arc<Shared>,
    wake: SyncSender<()>,
    clock: Arc<MasterClock>,
    width: u32,
    height: u32,
    stride: Arc<AtomicI32>,
}
impl IMFSourceReaderCallback_Impl for ReaderCallback_Impl {
    fn OnReadSample(
        &self,
        status: HRESULT,
        _stream: u32,
        flags: u32,
        _sample_time: i64,
        sample: Ref<'_, IMFSample>,
    ) -> windows::core::Result<()> {
        if self.shared.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        let outcome = if status.is_err() {
            Err(error(
                if status == E_ACCESSDENIED {
                    ErrorCode::PermissionDenied
                } else {
                    ErrorCode::SourceLost
                },
                format!(
                    "Camera frame delivery failed with HRESULT 0x{:08X}.",
                    status.0
                ),
            ))
        } else if flags
            & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32
                | MF_SOURCE_READERF_ERROR.0 as u32
                | MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32
                | MF_SOURCE_READERF_NATIVEMEDIATYPECHANGED.0 as u32)
            != 0
        {
            Err(error(
                ErrorCode::SourceLost,
                format!("Camera stream ended or changed format (flags 0x{flags:08X})."),
            ))
        } else if let Some(sample) = sample.as_ref() {
            copy_sample(
                sample,
                self.width,
                self.height,
                self.stride.load(Ordering::Acquire),
                self.clock.now(),
            )
        } else {
            Ok(None)
        };
        match outcome {
            Ok(Some(frame)) => {
                *self
                    .shared
                    .latest
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner()) = Some(frame);
                self.shared.delivered.fetch_add(1, Ordering::Release);
            }
            Ok(None) => {}
            Err(e) => {
                let _ = self.shared.terminal.set(e);
            }
        }
        let _ = self.wake.try_send(());
        Ok(())
    }
    fn OnFlush(&self, _stream: u32) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnEvent(&self, _stream: u32, _event: Ref<'_, IMFMediaEvent>) -> windows::core::Result<()> {
        Ok(())
    }
}

fn copy_sample(
    sample: &IMFSample,
    width: u32,
    height: u32,
    stride: i32,
    timestamp: MediaTimestamp,
) -> Result<Option<CameraFrame>, RecorderError> {
    let row = usize::try_from(width)
        .unwrap_or(usize::MAX)
        .checked_mul(4)
        .ok_or_else(|| RecorderError::invalid("Camera row size overflowed."))?;
    let bytes = row
        .checked_mul(height as usize)
        .ok_or_else(|| RecorderError::invalid("Camera frame size overflowed."))?;
    if bytes == 0 || bytes > MAX_CAMERA_BYTES || (stride.unsigned_abs() as usize) < row {
        return Err(RecorderError::invalid(
            "Camera RGB32 frame or stride exceeds the supported bounded format.",
        ));
    }
    let buffer = unsafe { sample.ConvertToContiguousBuffer() }
        .map_err(|e| native("read frame buffer", e))?;
    let mut data = ptr::null_mut();
    let mut length = 0u32;
    unsafe { buffer.Lock(&mut data, None, Some(&mut length)) }
        .map_err(|e| native("lock frame buffer", e))?;
    let copied = (|| {
        let pitch = stride.unsigned_abs() as usize;
        let needed = pitch
            .checked_mul(height as usize)
            .ok_or_else(|| RecorderError::invalid("Camera buffer stride overflowed."))?;
        if data.is_null() || (length as usize) < needed {
            return Err(error(
                ErrorCode::SourceLost,
                "Camera delivered a truncated RGB32 frame.",
            ));
        }
        let mut bgra = vec![0u8; bytes];
        let source = unsafe { std::slice::from_raw_parts(data, length as usize) };
        for y in 0..height as usize {
            let source_y = if stride > 0 {
                height as usize - 1 - y
            } else {
                y
            };
            bgra[y * row..(y + 1) * row]
                .copy_from_slice(&source[source_y * pitch..source_y * pitch + row]);
        }
        Ok(Some(CameraFrame {
            timestamp,
            width,
            height,
            bgra,
        }))
    })();
    let unlock = unsafe { buffer.Unlock() }.map_err(|e| native("unlock frame buffer", e));
    unlock?;
    copied
}

pub struct CameraSession {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    width: u32,
    height: u32,
}

impl CameraSession {
    pub fn start(
        selection: &CameraSelection,
        clock: Arc<MasterClock>,
    ) -> Result<Self, RecorderError> {
        if selection.device_id.is_empty() {
            return Err(RecorderError::invalid("Select a camera before opening it."));
        }
        let shared = Arc::new(Shared {
            latest: Mutex::new(None),
            terminal: OnceLock::new(),
            stopped: AtomicBool::new(false),
            delivered: AtomicU64::new(0),
        });
        let (ready, response) = sync_channel(1);
        let choice = selection.clone();
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("frame-camera".into())
            .spawn(move || camera_worker(choice, clock, worker_shared, ready))
            .map_err(|e| {
                error(
                    ErrorCode::Internal,
                    format!("Camera worker could not start: {e}"),
                )
            })?;
        let (width, height) = match response.recv_timeout(START_TIMEOUT) {
            Ok(Ok(size)) => size,
            Ok(Err(e)) => {
                let _ = worker.join();
                return Err(e);
            }
            Err(_) => {
                shared.stopped.store(true, Ordering::Release);
                let _ = worker.join();
                return Err(error(
                    ErrorCode::SourceLost,
                    "Camera did not open within five seconds.",
                ));
            }
        };
        Ok(Self {
            shared,
            worker: Some(worker),
            width,
            height,
        })
    }
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    pub fn try_latest(&self) -> Result<Option<CameraFrame>, RecorderError> {
        if let Some(error) = self.shared.terminal.get() {
            return Err(error.clone());
        }
        Ok(self
            .shared
            .latest
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take())
    }
    pub fn wait_first_frame(&self) -> Result<CameraFrame, RecorderError> {
        let started = Instant::now();
        loop {
            if let Some(frame) = self.try_latest()? {
                return Ok(frame);
            }
            if started.elapsed() >= FIRST_FRAME_TIMEOUT {
                return Err(error(
                    ErrorCode::SourceLost,
                    "Selected camera delivered no frame within five seconds.",
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    pub fn stop(&mut self) -> Result<(), RecorderError> {
        self.shared.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| error(ErrorCode::Internal, "Camera worker panicked during stop."))?;
        }
        *self
            .shared
            .latest
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = None;
        Ok(())
    }
}
impl Drop for CameraSession {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn camera_worker(
    selection: CameraSelection,
    clock: Arc<MasterClock>,
    shared: Arc<Shared>,
    ready: SyncSender<Result<(u32, u32), RecorderError>>,
) {
    let outcome = (|| -> Result<(), RecorderError> {
        let _runtime = MfRuntime::new()?;
        let activation = selected_activation(&selection.device_id)?;
        let source: IMFMediaSource = unsafe { activation.ActivateObject() }
            .map_err(|e| native("activate selected device", e))?;
        let outcome = (|| -> Result<(), RecorderError> {
            let (wake, receiver) = sync_channel(1);
            // The native format is selected by its enumerated index. The reader may
            // convert that one format to RGB32; a failed conversion is surfaced.
            let probe =
                unsafe { MFCreateSourceReaderFromMediaSource(&source, None::<&IMFAttributes>) }
                    .map_err(|e| native("open format reader", e))?;
            let (native_type, format) =
                format_at(&probe, selection.format_index)?.ok_or_else(|| {
                    RecorderError::invalid("The selected camera format no longer exists.")
                })?;
            if !format.supported || format.width == 0 || format.height == 0 {
                return Err(RecorderError::invalid(
                    "The selected camera format is unsupported.",
                ));
            }
            let row = format
                .width
                .checked_mul(4)
                .ok_or_else(|| RecorderError::invalid("Camera row size overflowed."))?;
            if u64::from(row) * u64::from(format.height) > MAX_CAMERA_BYTES as u64 {
                return Err(RecorderError::invalid(
                    "Selected camera frame exceeds the 64 MiB latest-frame bound.",
                ));
            }
            drop(probe);
            let applied_stride = Arc::new(AtomicI32::new(-(row as i32)));
            let callback: IMFSourceReaderCallback = ReaderCallback {
                shared: Arc::clone(&shared),
                wake,
                clock,
                width: format.width,
                height: format.height,
                stride: Arc::clone(&applied_stride),
            }
            .into();
            let mut attributes = None;
            unsafe { MFCreateAttributes(&mut attributes, 2) }
                .map_err(|e| native("create reader attributes", e))?;
            let attributes = attributes.ok_or_else(|| {
                error(
                    ErrorCode::Internal,
                    "Camera reader attributes were not created.",
                )
            })?;
            unsafe {
                attributes
                    .SetUnknown(&MF_SOURCE_READER_ASYNC_CALLBACK, &callback)
                    .map_err(|e| native("set async callback", e))?;
                attributes
                    .SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)
                    .map_err(|e| native("enable video conversion", e))?;
            }
            let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source, &attributes) }
                .map_err(|e| native("open async reader", e))?;
            // SetNativeMediaType fixes the device mode. SetCurrentMediaType below
            // only selects the reader's converted output format.
            let reader_ex: IMFSourceReaderEx = reader
                .cast()
                .map_err(|e| native("access native format selection", e))?;
            unsafe { reader_ex.SetNativeMediaType(VIDEO_STREAM, &native_type) }
                .map_err(|e| native("select native format", e))?;
            let native_applied = unsafe {
                reader
                    .GetNativeMediaType(VIDEO_STREAM, MF_SOURCE_READER_CURRENT_TYPE_INDEX.0 as u32)
            }
            .map_err(|e| native("verify native format", e))?;
            let native_subtype = unsafe { native_applied.GetGUID(&MF_MT_SUBTYPE) }
                .map_err(|e| native("verify native subtype", e))?;
            let native_size = unsafe { native_applied.GetUINT64(&MF_MT_FRAME_SIZE) }
                .map_err(|e| native("verify native frame size", e))?;
            let native_rate = unsafe { native_applied.GetUINT64(&MF_MT_FRAME_RATE) }
                .map_err(|e| native("verify native frame rate", e))?;
            if native_subtype
                != unsafe { native_type.GetGUID(&MF_MT_SUBTYPE) }
                    .map_err(|e| native("read selected subtype", e))?
                || native_size
                    != unsafe { native_type.GetUINT64(&MF_MT_FRAME_SIZE) }
                        .map_err(|e| native("read selected frame size", e))?
                || native_rate
                    != unsafe { native_type.GetUINT64(&MF_MT_FRAME_RATE) }
                        .map_err(|e| native("read selected frame rate", e))?
            {
                return Err(RecorderError::invalid(
                    "The camera did not apply the selected native format.",
                ));
            }
            let output =
                unsafe { MFCreateMediaType() }.map_err(|e| native("create RGB32 type", e))?;
            unsafe {
                native_type
                    .CopyAllItems(&output)
                    .map_err(|e| native("copy native format", e))?;
                output
                    .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
                    .map_err(|e| native("set RGB32 subtype", e))?;
                output
                    .SetUINT32(&MF_MT_DEFAULT_STRIDE, (-(row as i32)) as u32)
                    .map_err(|e| native("set RGB32 stride", e))?;
                reader
                    .SetCurrentMediaType(VIDEO_STREAM, None, &output)
                    .map_err(|e| error(ErrorCode::InvalidRequest,
                        format!("Selected camera format cannot convert to RGB32 (HRESULT 0x{:08X}).", e.code().0)))?;
            }
            let applied = unsafe { reader.GetCurrentMediaType(VIDEO_STREAM) }
                .map_err(|e| native("verify RGB32 type", e))?;
            let native_after_conversion = unsafe {
                reader
                    .GetNativeMediaType(VIDEO_STREAM, MF_SOURCE_READER_CURRENT_TYPE_INDEX.0 as u32)
            }
            .map_err(|e| native("verify device format after conversion", e))?;
            if unsafe { native_after_conversion.GetGUID(&MF_MT_SUBTYPE) }
                .map_err(|e| native("verify device subtype after conversion", e))?
                != native_subtype
                || unsafe { native_after_conversion.GetUINT64(&MF_MT_FRAME_SIZE) }
                    .map_err(|e| native("verify device geometry after conversion", e))?
                    != native_size
                || unsafe { native_after_conversion.GetUINT64(&MF_MT_FRAME_RATE) }
                    .map_err(|e| native("verify device rate after conversion", e))?
                    != native_rate
            {
                return Err(RecorderError::invalid(
                    "Camera conversion changed the selected native device format.",
                ));
            }
            let subtype = unsafe { applied.GetGUID(&MF_MT_SUBTYPE) }
                .map_err(|e| native("verify RGB32 subtype", e))?;
            let size = unsafe { applied.GetUINT64(&MF_MT_FRAME_SIZE) }
                .map_err(|e| native("verify camera geometry", e))?;
            let rate = unsafe { applied.GetUINT64(&MF_MT_FRAME_RATE) }
                .map_err(|e| native("verify camera frame rate", e))?;
            let stride = unsafe { applied.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
                .map_err(|e| native("verify camera stride", e))? as i32;
            if subtype != MFVideoFormat_RGB32
                || (size >> 32) as u32 != format.width
                || size as u32 != format.height
                || rate != native_rate
                || stride.unsigned_abs() != row
            {
                return Err(RecorderError::invalid(
                    "Camera RGB32 conversion changed subtype, geometry, rate or stride; the format was not opened.",
                ));
            }
            applied_stride.store(stride, Ordering::Release);
            unsafe { reader.ReadSample(VIDEO_STREAM, 0, None, None, None, None) }
                .map_err(|e| native("request first camera frame", e))?;
            let _ = ready.send(Ok((format.width, format.height)));
            let mut last_delivery = Instant::now();
            let mut last_count = 0;
            while !shared.stopped.load(Ordering::Acquire) && shared.terminal.get().is_none() {
                match receiver.recv_timeout(Duration::from_millis(20)) {
                    Ok(()) => {
                        let count = shared.delivered.load(Ordering::Acquire);
                        if count != last_count {
                            last_count = count;
                            last_delivery = Instant::now();
                        }
                        if !shared.stopped.load(Ordering::Acquire)
                            && shared.terminal.get().is_none()
                        {
                            unsafe { reader.ReadSample(VIDEO_STREAM, 0, None, None, None, None) }
                                .map_err(|e| native("request camera frame", e))?;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if last_delivery.elapsed() >= FIRST_FRAME_TIMEOUT {
                            return Err(error(
                                ErrorCode::SourceLost,
                                "Selected camera stopped delivering frames for five seconds.",
                            ));
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            unsafe {
                let _ = reader.Flush(VIDEO_STREAM);
            }
            Ok(())
        })();
        unsafe {
            let _ = source.Shutdown();
        }
        outcome
    })();
    if let Err(error) = outcome {
        let _ = ready.try_send(Err(error.clone()));
        let _ = shared.terminal.set(error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frame_contracts::CameraPlacement;

    #[test]
    fn no_selection_never_starts_a_worker() {
        let choice = CameraSelection {
            device_id: String::new(),
            format_index: 0,
            placement: CameraPlacement {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            },
        };
        assert!(CameraSession::start(&choice, Arc::new(MasterClock::default())).is_err());
    }

    #[test]
    fn latest_frame_is_replaced_and_stop_clears_it() {
        let shared = Arc::new(Shared {
            latest: Mutex::new(None),
            terminal: OnceLock::new(),
            stopped: AtomicBool::new(false),
            delivered: AtomicU64::new(0),
        });
        let mut session = CameraSession {
            shared: Arc::clone(&shared),
            worker: None,
            width: 2,
            height: 2,
        };
        for value in [1, 2] {
            *shared.latest.lock().unwrap() = Some(CameraFrame {
                timestamp: MediaTimestamp(value),
                width: 2,
                height: 2,
                bgra: vec![value as u8; 16],
            });
        }
        assert_eq!(
            session.try_latest().unwrap().unwrap().timestamp,
            MediaTimestamp(2)
        );
        assert!(session.try_latest().unwrap().is_none());
        *shared.latest.lock().unwrap() = Some(CameraFrame {
            timestamp: MediaTimestamp(3),
            width: 2,
            height: 2,
            bgra: vec![3; 16],
        });
        session.stop().unwrap();
        assert!(session.try_latest().unwrap().is_none());
        session.stop().unwrap();
    }
}
